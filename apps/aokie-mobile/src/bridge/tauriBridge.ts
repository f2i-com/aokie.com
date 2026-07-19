import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { parseCallSnapshot, parseCommandAck } from "../protocol/codec";
import { MAX_JSON_SAFE_INTEGER, type CommandEnvelope } from "../protocol/types";
import type {
  BridgeEvent,
  CompanionAvailability,
  CompanionAvailabilityRecord,
  CompanionAvailabilityState,
  CompanionBridge,
  CompanionBootstrap,
  CompanionCallRecord,
  CompanionCallRecordDetail,
  CompanionCallRecords,
  CompanionCapability,
  CustomServerAuthorization,
  CompanionHistory,
  CompanionPushEndpoint,
  CompanionRouting,
  CompanionRoutingGroup,
  CompanionStaffMember,
  DesktopPairingConfirmation,
  DesktopPairingDecision,
  DesktopPairingReview,
  DesktopPeerTrustChallenge,
  ForgetServerProfileResult,
  DiscoveryDocument,
  LocalMediaProof,
  ManagedAdmissionState,
  NativeAudioDevice,
  NativeAudioDevices,
  NativeIceServer,
  NativeIceCandidate,
  NativeMediaOffer,
  NativeMediaOfferRequest,
  NativeMediaSession,
  NativeMediaSignal,
  NativeMediaSignalEvent,
  NativeMediaStateEvent,
  NativeMediaLevelsEvent,
  NativeSdpSignal,
  RealtimeConfig,
  RuntimeCapabilities,
  ServerProfile,
  SyncReady,
  V2CallSnapshotEvent,
  V2AssistanceAnswerAcceptedEvent,
  V2AssistanceRequestEvent,
  V2EndCallerEvent,
  V2Grant,
  V2IdleSyncEvent,
  V2LeaseEvent,
  V2LeaseMode,
  V2RequestReceipt,
} from "./CompanionBridge";

type Listener = (event: BridgeEvent) => void;
const SAFE_ID = /^[A-Za-z0-9._:-]{1,200}$/;
const RFC3339 = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/;
const MEDIA_PHASE = /^[a-z][a-z0-9_]{0,63}$/;
const MEDIA_MODES = new Set(["monitor", "prepared_consult", "prepared_talk", "consult", "talk"]);
const V2_GRANTS = new Set<V2Grant>(["state_read", "caller_read", "captions_read", "assistance_read", "assistance_respond", "monitor", "consult", "takeover", "resume_aokie", "rtc_signal", "end_caller", "participants_read", "participant_identity_read", "audio_levels_read"]);
const V2_LEASE_MODES = new Set<V2LeaseMode>(["monitor", "consult", "takeover"]);
const MAX_SDP_BYTES = 128 * 1024;
const MAX_CANDIDATE_BYTES = 8 * 1024;
const MAX_AUDIO_DEVICES_PER_KIND = 128;
const DB_TIMESTAMP = /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/;
const COMPANION_CAPABILITIES = new Set<CompanionCapability>([
  "state_read", "caller_read", "captions_read", "monitor", "consult", "takeover",
  "resume_aokie", "rtc_signal", "assistance_read", "assistance_respond", "end_caller",
  "participants_read", "participant_identity_read", "audio_levels_read",
]);
const COMPANION_AVAILABILITY = new Set<CompanionAvailabilityState>(["available", "busy", "offline", "do_not_disturb"]);
const COMPANION_SESSION_MODES = new Set(["monitor", "consult", "takeover"]);
const COMPANION_ACTIVITY_TYPES = new Set([
  "admission_issued", "monitor_joined", "monitor_left", "consult_joined", "consult_left",
  "takeover_prepared", "takeover_joined", "takeover_left", "returned_to_aokie",
  "session_recovered", "session_revoked", "endpoint_revoked", "call_alert_targeted",
  "assistance_targeted", "takeover_targeted",
]);
const ENDPOINT_THUMBPRINT = /^[A-Za-z0-9_-]{43}$/;
const DISPLAY_FINGERPRINT = /^(?:[0-9A-F]{4}:){15}[0-9A-F]{4}$/;
const MAX_PAIRING_JSON_BYTES = 16 * 1024;

// Native media state is a heartbeat, not a durable assertion. The native
// bridge should use a five-second refresh cadence; proofs with more than ten
// seconds of lifetime remaining at receipt are rejected by the renderer.
export const LOCAL_MEDIA_PROOF_MAX_TTL_MS = 10_000;

export function parseLocalMediaProof(value: unknown, receivedAtMs = Date.now()): LocalMediaProof | null {
  if (value === null) return null;
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("Invalid local media proof");
  }
  const proof = value as Record<string, unknown>;
  const keys = Object.keys(proof);
  if (keys.some((key) => ![
    "appId", "streamNonce", "rtcSessionId", "callId", "callEpoch", "ownerEpoch", "deviceId", "leaseId", "mode", "fence",
    "sdpRevision", "transportGeneration", "expiresAt", "active",
  ].includes(key))) {
    throw new Error("Invalid local media proof");
  }
  const expiresAtMs = typeof proof.expiresAt === "string" ? Date.parse(proof.expiresAt) : Number.NaN;
  if (
    typeof proof.appId !== "string" || !SAFE_ID.test(proof.appId) ||
    typeof proof.streamNonce !== "string" || !SAFE_ID.test(proof.streamNonce) ||
    typeof proof.rtcSessionId !== "string" || !SAFE_ID.test(proof.rtcSessionId) ||
    typeof proof.callId !== "string" || !SAFE_ID.test(proof.callId) ||
    !Number.isSafeInteger(proof.callEpoch) || Number(proof.callEpoch) < 0 || Number(proof.callEpoch) > MAX_JSON_SAFE_INTEGER ||
    !Number.isSafeInteger(proof.ownerEpoch) || Number(proof.ownerEpoch) < 0 || Number(proof.ownerEpoch) > MAX_JSON_SAFE_INTEGER ||
    typeof proof.deviceId !== "string" || !SAFE_ID.test(proof.deviceId) ||
    typeof proof.leaseId !== "string" || !SAFE_ID.test(proof.leaseId) ||
    (proof.mode !== "consult" && proof.mode !== "talk") ||
    !Number.isSafeInteger(proof.fence) || Number(proof.fence) < 0 || Number(proof.fence) > MAX_JSON_SAFE_INTEGER ||
    (proof.mode === "consult" ? Number(proof.fence) !== 0 : Number(proof.fence) < 1) ||
    !Number.isSafeInteger(proof.sdpRevision) || Number(proof.sdpRevision) < 1 || Number(proof.sdpRevision) > MAX_JSON_SAFE_INTEGER ||
    !Number.isSafeInteger(proof.transportGeneration) || Number(proof.transportGeneration) < 1 || Number(proof.transportGeneration) > MAX_JSON_SAFE_INTEGER ||
    typeof proof.expiresAt !== "string" || proof.expiresAt.length > 64 || !RFC3339.test(proof.expiresAt) ||
    !Number.isFinite(receivedAtMs) || !Number.isFinite(expiresAtMs) || expiresAtMs <= receivedAtMs ||
    expiresAtMs - receivedAtMs > LOCAL_MEDIA_PROOF_MAX_TTL_MS ||
    proof.active !== true
  ) {
    throw new Error("Invalid local media proof");
  }
  return proof as unknown as LocalMediaProof;
}

function record(value: unknown, name: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error(`Invalid ${name}`);
  return value as Record<string, unknown>;
}

function exactKeys(value: Record<string, unknown>, allowed: readonly string[], name: string): void {
  if (Object.keys(value).some((key) => !allowed.includes(key))) throw new Error(`Invalid ${name}`);
}

function safeInteger(value: unknown, minimum: number, name: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < minimum || Number(value) > MAX_JSON_SAFE_INTEGER) {
    throw new Error(`Invalid ${name}`);
  }
  return Number(value);
}

function safeIdentifier(value: unknown, name: string): string {
  if (typeof value !== "string" || !SAFE_ID.test(value)) throw new Error(`Invalid ${name}`);
  return value;
}

function audioDeviceId(value: unknown, name: string): string {
  if (typeof value !== "string" || !value || value.length > 1_024 || /[\u0000-\u001f\u007f]/.test(value)) {
    throw new Error(`Invalid ${name}`);
  }
  return value;
}

function timestamp(value: unknown, name: string): string {
  if (typeof value !== "string" || value.length > 64 || !RFC3339.test(value) || !Number.isFinite(Date.parse(value))) {
    throw new Error(`Invalid ${name}`);
  }
  return value;
}

export function parseNativeMediaSession(value: unknown): NativeMediaSession {
  const session = record(value, "native media session");
  exactKeys(session, [
    "appId", "streamNonce", "rtcSessionId", "callId", "callEpoch", "ownerEpoch", "deviceId", "mode", "leaseId",
    "fence", "sdpRevision", "transportGeneration", "expiresAt",
  ], "native media session");
  const mode = session.mode;
  if (typeof mode !== "string" || !MEDIA_MODES.has(mode)) throw new Error("Invalid native media mode");
  const fence = safeInteger(session.fence, 0, "native media fence");
  if (((mode === "talk" || mode === "prepared_talk") && fence < 1) || ((mode === "monitor" || mode === "prepared_consult" || mode === "consult") && fence !== 0)) {
    throw new Error("Invalid native media fence for mode");
  }
  return {
    appId: safeIdentifier(session.appId, "native media app"),
    streamNonce: safeIdentifier(session.streamNonce, "native media stream"),
    rtcSessionId: safeIdentifier(session.rtcSessionId, "native media RTC session"),
    callId: safeIdentifier(session.callId, "native media call"),
    callEpoch: safeInteger(session.callEpoch, 0, "native media call epoch"),
    ownerEpoch: safeInteger(session.ownerEpoch, 0, "native media owner epoch"),
    deviceId: safeIdentifier(session.deviceId, "native media device"),
    mode: mode as NativeMediaSession["mode"],
    leaseId: safeIdentifier(session.leaseId, "native media lease"),
    fence,
    sdpRevision: safeInteger(session.sdpRevision, 1, "native media SDP revision"),
    transportGeneration: safeInteger(session.transportGeneration, 1, "native media transport generation"),
    expiresAt: timestamp(session.expiresAt, "native media expiry"),
  };
}

export function parseNativeSdpSignal(value: unknown): NativeSdpSignal {
  const signal = record(value, "native SDP signal");
  exactKeys(signal, ["type", "sdp"], "native SDP signal");
  if ((signal.type !== "offer" && signal.type !== "answer") || typeof signal.sdp !== "string" || !signal.sdp.startsWith("v=0") || signal.sdp.length > MAX_SDP_BYTES || signal.sdp.includes("\0")) {
    throw new Error("Invalid native SDP signal");
  }
  return signal as unknown as NativeSdpSignal;
}

export function parseNativeIceCandidate(value: unknown): NativeIceCandidate {
  const candidate = record(value, "native ICE candidate");
  exactKeys(candidate, ["sdpMid", "sdpMlineIndex", "candidate"], "native ICE candidate");
  if (
    typeof candidate.sdpMid !== "string" || candidate.sdpMid.length > 64 || [...candidate.sdpMid].some((char) => /[\u0000-\u001f\u007f]/.test(char)) ||
    !Number.isInteger(candidate.sdpMlineIndex) || Number(candidate.sdpMlineIndex) < 0 || Number(candidate.sdpMlineIndex) > 64 ||
    typeof candidate.candidate !== "string" || !candidate.candidate || candidate.candidate.length > MAX_CANDIDATE_BYTES || candidate.candidate.includes("\0")
  ) throw new Error("Invalid native ICE candidate");
  return candidate as unknown as NativeIceCandidate;
}

export function parseNativeMediaSignalEvent(value: unknown): NativeMediaSignalEvent {
  const event = record(value, "native media signal event");
  exactKeys(event, ["session", "signal"], "native media signal event");
  const rawSignal = record(event.signal, "native media signal");
  let signal: NativeMediaSignal;
  if (rawSignal.kind === "offer") {
    exactKeys(rawSignal, ["kind", "description"], "native media offer signal");
    const description = parseNativeSdpSignal(rawSignal.description);
    if (description.type !== "offer") throw new Error("Invalid native media offer signal");
    signal = { kind: "offer", description };
  } else if (rawSignal.kind === "ice") {
    exactKeys(rawSignal, ["kind", "candidate"], "native media ICE signal");
    signal = { kind: "ice", candidate: parseNativeIceCandidate(rawSignal.candidate) };
  } else if (rawSignal.kind === "ice_complete") {
    exactKeys(rawSignal, ["kind"], "native media ICE-complete signal");
    signal = { kind: "ice_complete" };
  } else {
    throw new Error("Invalid native media signal");
  }
  return { session: parseNativeMediaSession(event.session), signal };
}

export function parseNativeMediaStateEvent(value: unknown): NativeMediaStateEvent {
  const event = record(value, "native media state event");
  exactKeys(event, ["session", "phase", "microphoneActive", "remoteAudioReady", "reason"], "native media state event");
  if (
    typeof event.phase !== "string" || !MEDIA_PHASE.test(event.phase) ||
    typeof event.microphoneActive !== "boolean" || typeof event.remoteAudioReady !== "boolean" ||
    (event.reason !== undefined && (typeof event.reason !== "string" || event.reason.length > 200 || /[\u0000-\u001f\u007f]/.test(event.reason)))
  ) throw new Error("Invalid native media state event");
  return {
    session: parseNativeMediaSession(event.session),
    phase: event.phase,
    microphoneActive: event.microphoneActive,
    remoteAudioReady: event.remoteAudioReady,
    ...(typeof event.reason === "string" ? { reason: event.reason } : {}),
  };
}

export function parseNativeMediaLevelsEvent(value: unknown): NativeMediaLevelsEvent | null {
  if (value === null) return null;
  const event = record(value, "native media levels event");
  exactKeys(event, ["session", "microphoneLevelPermille", "remoteLevelPermille", "measuredAt"], "native media levels event");
  const parseLevel = (level: unknown, label: string): number | undefined => {
    if (level === undefined) return undefined;
    if (!Number.isInteger(level) || Number(level) < 0 || Number(level) > 1_000) {
      throw new Error(`Invalid native ${label} level`);
    }
    return Number(level);
  };
  return {
    session: parseNativeMediaSession(event.session),
    microphoneLevelPermille: parseLevel(event.microphoneLevelPermille, "microphone"),
    remoteLevelPermille: parseLevel(event.remoteLevelPermille, "remote audio"),
    measuredAt: timestamp(event.measuredAt, "native media level measurement"),
  };
}

export function parseNativeAudioDevices(value: unknown): NativeAudioDevices {
  const response = record(value, "native audio devices");
  exactKeys(response, [
    "routingPolicy", "inputDevices", "outputDevices", "selectedInputId", "selectedOutputId", "state", "canSelect",
  ], "native audio devices");
  if (
    !["selectable", "system_managed", "unavailable"].includes(String(response.routingPolicy)) ||
    !["idle", "media_active", "microphone_active"].includes(String(response.state)) ||
    typeof response.canSelect !== "boolean" || !Array.isArray(response.inputDevices) || !Array.isArray(response.outputDevices) ||
    response.inputDevices.length > MAX_AUDIO_DEVICES_PER_KIND || response.outputDevices.length > MAX_AUDIO_DEVICES_PER_KIND
  ) throw new Error("Invalid native audio devices");

  const parseDevices = (values: unknown[], kind: string): NativeAudioDevice[] => {
    const seen = new Set<string>();
    return values.map((value) => {
      const device = record(value, `native ${kind} device`);
      exactKeys(device, ["id", "label"], `native ${kind} device`);
      const id = audioDeviceId(device.id, `native ${kind} device ID`);
      if (seen.has(id) || typeof device.label !== "string" || !device.label.trim() || device.label.length > 200 || /[\u0000-\u001f\u007f]/.test(device.label)) {
        throw new Error(`Invalid native ${kind} device`);
      }
      seen.add(id);
      return { id, label: device.label };
    });
  };
  const inputDevices = parseDevices(response.inputDevices, "input");
  const outputDevices = parseDevices(response.outputDevices, "output");
  const selectedInputId = audioDeviceId(response.selectedInputId, "selected input device ID");
  const selectedOutputId = audioDeviceId(response.selectedOutputId, "selected output device ID");
  const selectable = response.routingPolicy === "selectable";
  const systemManaged = response.routingPolicy === "system_managed";
  const managedRoutesMatch = systemManaged &&
    inputDevices.length === outputDevices.length &&
    inputDevices.every((device, index) => {
      const output = outputDevices[index];
      return output?.id === device.id && output.label === device.label;
    });
  const managedSelectedRouteExists = selectedInputId === "system_managed" ||
    outputDevices.some((device) => device.id === selectedInputId);
  const managedCanSelect = response.state !== "idle" && outputDevices.length > 0;
  if (
    (selectable && response.canSelect !== (response.state === "idle")) ||
    (selectable && (
      !inputDevices.some((device) => device.id === "system_default") ||
      !outputDevices.some((device) => device.id === "system_default")
    )) ||
    (systemManaged && (
      !managedRoutesMatch || selectedInputId !== selectedOutputId || !managedSelectedRouteExists ||
      response.canSelect !== managedCanSelect
    )) ||
    (response.routingPolicy === "unavailable" && (
      inputDevices.length !== 0 || outputDevices.length !== 0 || response.canSelect ||
      selectedInputId !== "unavailable" || selectedOutputId !== "unavailable"
    ))
  ) throw new Error("Invalid native audio routing state");
  return {
    routingPolicy: response.routingPolicy as NativeAudioDevices["routingPolicy"],
    inputDevices,
    outputDevices,
    selectedInputId,
    selectedOutputId,
    state: response.state as NativeAudioDevices["state"],
    canSelect: response.canSelect,
  };
}

function parseNativeMediaOffer(value: unknown): NativeMediaOffer {
  const offer = record(value, "native media offer");
  exactKeys(offer, ["session", "offer"], "native media offer");
  const description = parseNativeSdpSignal(offer.offer);
  if (description.type !== "offer") throw new Error("Invalid native media offer");
  return { session: parseNativeMediaSession(offer.session), offer: description };
}

export function parseSyncReady(value: unknown): SyncReady {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("Invalid realtime sync proof");
  }
  const proof = value as Record<string, unknown>;
  if (
    Object.keys(proof).some((key) => !["streamNonce", "sequence"].includes(key)) ||
    typeof proof.streamNonce !== "string" || !SAFE_ID.test(proof.streamNonce) ||
    !Number.isSafeInteger(proof.sequence) || Number(proof.sequence) < 0 || Number(proof.sequence) > MAX_JSON_SAFE_INTEGER
  ) {
    throw new Error("Invalid realtime sync proof");
  }
  return proof as unknown as SyncReady;
}

export function parseDesktopPeerTrustChallenge(value: unknown, receivedAtSeconds = Math.floor(Date.now() / 1_000)): DesktopPeerTrustChallenge {
  const challenge = record(value, "Desktop peer trust challenge");
  exactKeys(challenge, [
    "challengeId", "profileId", "appId", "deviceId", "peerFingerprint", "previousPeerFingerprint", "rotation", "expiresAt",
  ], "Desktop peer trust challenge");
  const parsed: DesktopPeerTrustChallenge = {
    challengeId: safeIdentifier(challenge.challengeId, "Desktop peer trust challenge ID"),
    profileId: safeIdentifier(challenge.profileId, "Desktop peer trust profile ID"),
    appId: safeIdentifier(challenge.appId, "Desktop peer trust app ID"),
    deviceId: safeIdentifier(challenge.deviceId, "Desktop peer trust device ID"),
    peerFingerprint: safeIdentifier(challenge.peerFingerprint, "Desktop peer fingerprint"),
    rotation: challenge.rotation === true,
    expiresAt: safeInteger(challenge.expiresAt, 1, "Desktop peer trust expiry"),
  };
  if (challenge.rotation !== true && challenge.rotation !== false) throw new Error("Invalid Desktop peer trust rotation state");
  if (challenge.previousPeerFingerprint !== undefined) {
    parsed.previousPeerFingerprint = safeIdentifier(challenge.previousPeerFingerprint, "previous Desktop peer fingerprint");
  }
  if (parsed.rotation !== (parsed.previousPeerFingerprint !== undefined) || parsed.previousPeerFingerprint === parsed.peerFingerprint ||
      !Number.isSafeInteger(receivedAtSeconds) || parsed.expiresAt <= receivedAtSeconds || parsed.expiresAt > receivedAtSeconds + 301) {
    throw new Error("Invalid Desktop peer trust challenge");
  }
  return parsed;
}

export function parseManagedAdmissionState(value: unknown): ManagedAdmissionState {
  const state = record(value, "managed admission state");
  exactKeys(state, ["value", "code", "message"], "managed admission state");
  if (!["pairing_required", "desktop_unavailable", "policy_denied"].includes(String(state.value))) {
    throw new Error("Invalid managed admission state");
  }
  const code = safeIdentifier(state.code, "managed admission code");
  if (typeof state.message !== "string" || !state.message.trim() || state.message.length > 1_000 || /[\u0000-\u001f\u007f]/.test(state.message)) {
    throw new Error("Invalid managed admission message");
  }
  if ((state.value === "pairing_required") !== (code === "mobile_not_paired") ||
      (state.value === "desktop_unavailable") !== (code === "desktop_identity_unavailable")) {
    throw new Error("Invalid managed admission mapping");
  }
  return { value: state.value as ManagedAdmissionState["value"], code, message: state.message.trim() };
}

export function parseDesktopPairingReview(value: unknown, receivedAtSeconds = Math.floor(Date.now() / 1_000)): DesktopPairingReview {
  const review = record(value, "Desktop pairing review");
  exactKeys(review, [
    "reviewId", "profileId", "appId", "workspaceId", "desktopConnectionId", "deviceId", "desktopKeyThumbprint",
    "desktopFingerprint", "mobileKeyThumbprint", "mobileFingerprint", "issuedAt", "expiresAt", "unsignedPublicOffer",
  ], "Desktop pairing review");
  const parsed: DesktopPairingReview = {
    reviewId: safeIdentifier(review.reviewId, "Desktop pairing review ID"),
    profileId: safeIdentifier(review.profileId, "Desktop pairing profile ID"),
    appId: safeIdentifier(review.appId, "Desktop pairing app ID"),
    desktopConnectionId: safeIdentifier(review.desktopConnectionId, "Desktop pairing connection ID"),
    deviceId: safeIdentifier(review.deviceId, "Desktop pairing device ID"),
    desktopKeyThumbprint: endpointThumbprint(review.desktopKeyThumbprint, "Desktop key thumbprint"),
    desktopFingerprint: displayFingerprint(review.desktopFingerprint, "Desktop fingerprint"),
    mobileKeyThumbprint: endpointThumbprint(review.mobileKeyThumbprint, "Companion key thumbprint"),
    mobileFingerprint: displayFingerprint(review.mobileFingerprint, "Companion fingerprint"),
    issuedAt: safeInteger(review.issuedAt, 1, "Desktop pairing issue time"),
    expiresAt: safeInteger(review.expiresAt, 1, "Desktop pairing expiry"),
    unsignedPublicOffer: true,
  };
  if (review.workspaceId !== undefined) parsed.workspaceId = safeIdentifier(review.workspaceId, "Desktop pairing workspace ID");
  if (review.unsignedPublicOffer !== true || parsed.desktopKeyThumbprint === parsed.mobileKeyThumbprint ||
      !Number.isSafeInteger(receivedAtSeconds) || parsed.issuedAt > receivedAtSeconds + 5 || parsed.expiresAt <= receivedAtSeconds ||
      parsed.expiresAt <= parsed.issuedAt || parsed.expiresAt - parsed.issuedAt > 600) {
    throw new Error("Invalid Desktop pairing review");
  }
  return parsed;
}

export function parseDesktopPairingDecision(value: unknown): DesktopPairingDecision {
  const decision = record(value, "Desktop pairing decision");
  exactKeys(decision, [
    "approved", "responseJson", "desktopKeyThumbprint", "mobileKeyThumbprint", "mobileFingerprint", "deviceId", "expiresAt",
  ], "Desktop pairing decision");
  if (decision.approved !== true && decision.approved !== false) throw new Error("Invalid Desktop pairing decision");
  const parsed: DesktopPairingDecision = {
    approved: decision.approved,
    desktopKeyThumbprint: endpointThumbprint(decision.desktopKeyThumbprint, "Desktop pairing result thumbprint"),
    mobileKeyThumbprint: endpointThumbprint(decision.mobileKeyThumbprint, "Companion pairing result thumbprint"),
    mobileFingerprint: displayFingerprint(decision.mobileFingerprint, "Companion pairing result fingerprint"),
    deviceId: safeIdentifier(decision.deviceId, "Desktop pairing result device ID"),
  };
  if (!parsed.approved) {
    if (decision.responseJson !== undefined || decision.expiresAt !== undefined) throw new Error("Invalid rejected Desktop pairing result");
    return parsed;
  }
  if (typeof decision.responseJson !== "string" || !decision.responseJson || decision.responseJson.length > MAX_PAIRING_JSON_BYTES) {
    throw new Error("Invalid Desktop pairing response JSON");
  }
  const response = parsePairingResponseJson(decision.responseJson);
  parsed.expiresAt = safeInteger(decision.expiresAt, 1, "Desktop pairing response expiry");
  if (response.desktopKeyThumbprint !== parsed.desktopKeyThumbprint || response.mobileKeyThumbprint !== parsed.mobileKeyThumbprint ||
      response.deviceId !== parsed.deviceId || response.expiresAt !== parsed.expiresAt) {
    throw new Error("Desktop pairing response does not match its native result");
  }
  parsed.responseJson = decision.responseJson;
  return parsed;
}

function parsePairingResponseJson(value: string): { desktopKeyThumbprint: string; mobileKeyThumbprint: string; deviceId: string; expiresAt: number } {
  let decoded: unknown;
  try { decoded = JSON.parse(value); }
  catch { throw new Error("Invalid Desktop pairing response JSON"); }
  const response = record(decoded, "Desktop pairing response");
  exactKeys(response, ["kind", "schemaVersion", "claims", "signature"], "Desktop pairing response");
  if (response.kind !== "aokie_mobile_pairing_response" || response.schemaVersion !== 2 ||
      typeof response.signature !== "string" || !/^[A-Za-z0-9_-]{86}$/.test(response.signature)) {
    throw new Error("Invalid Desktop pairing response envelope");
  }
  const claims = record(response.claims, "Desktop pairing response claims");
  exactKeys(claims, [
    "appId", "workspaceId", "desktopConnectionId", "desktopKeyThumbprint", "deviceId", "displayName",
    "mobileEndpointKey", "pairingNonce", "jti", "issuedAt", "expiresAt",
  ], "Desktop pairing response claims");
  for (const [field, label] of [
    ["appId", "app ID"], ["desktopConnectionId", "Desktop connection ID"], ["deviceId", "device ID"],
    ["pairingNonce", "pairing nonce"], ["jti", "pairing JTI"],
  ] as const) safeIdentifier(claims[field], `Desktop pairing response ${label}`);
  if (claims.workspaceId !== undefined) safeIdentifier(claims.workspaceId, "Desktop pairing response workspace ID");
  if (typeof claims.displayName !== "string" || !claims.displayName.trim() || claims.displayName.length > 120 || /[\u0000-\u001f\u007f]/.test(claims.displayName)) {
    throw new Error("Invalid Desktop pairing response display name");
  }
  const endpoint = record(claims.mobileEndpointKey, "Desktop pairing response endpoint key");
  exactKeys(endpoint, ["algorithm", "publicKey", "thumbprint"], "Desktop pairing response endpoint key");
  if (endpoint.algorithm !== "ed25519" || typeof endpoint.publicKey !== "string" || !ENDPOINT_THUMBPRINT.test(endpoint.publicKey)) {
    throw new Error("Invalid Desktop pairing response endpoint key");
  }
  const mobileKeyThumbprint = endpointThumbprint(endpoint.thumbprint, "Desktop pairing response mobile key thumbprint");
  const desktopKeyThumbprint = endpointThumbprint(claims.desktopKeyThumbprint, "Desktop pairing response Desktop key thumbprint");
  const issuedAt = safeInteger(claims.issuedAt, 1, "Desktop pairing response issue time");
  const expiresAt = safeInteger(claims.expiresAt, 1, "Desktop pairing response expiry");
  if (expiresAt <= issuedAt || expiresAt - issuedAt > 600 || desktopKeyThumbprint === mobileKeyThumbprint) {
    throw new Error("Invalid Desktop pairing response lifetime or key binding");
  }
  return { desktopKeyThumbprint, mobileKeyThumbprint, deviceId: safeIdentifier(claims.deviceId, "Desktop pairing response device ID"), expiresAt };
}

function endpointThumbprint(value: unknown, label: string): string {
  if (typeof value !== "string" || !ENDPOINT_THUMBPRINT.test(value)) throw new Error(`Invalid ${label}`);
  return value;
}

function displayFingerprint(value: unknown, label: string): string {
  if (typeof value !== "string" || !DISPLAY_FINGERPRINT.test(value)) throw new Error(`Invalid ${label}`);
  return value;
}

function profileUrl(value: unknown, name: string, originOnly = false): string {
  if (typeof value !== "string" || !value || value.length > 1_024 || /[\u0000-\u001f\u007f]/.test(value)) throw new Error(`Invalid ${name}`);
  let parsed: URL;
  try { parsed = new URL(value); }
  catch { throw new Error(`Invalid ${name}`); }
  if (!['https:', 'http:'].includes(parsed.protocol) || parsed.username || parsed.password || parsed.search || parsed.hash ||
      (originOnly && parsed.pathname !== "/")) throw new Error(`Invalid ${name}`);
  return value;
}

export function parseCustomServerAuthorization(value: unknown, receivedAtSeconds = Math.floor(Date.now() / 1_000)): CustomServerAuthorization {
  const authorization = record(value, "custom-server authorization");
  exactKeys(authorization, [
    "authorizationId", "profileId", "origin", "discoveryFingerprint", "endpointFingerprint", "previousDiscoveryFingerprint", "trustState", "expiresAt",
  ], "custom-server authorization");
  const trustState = authorization.trustState;
  if (!['first_use', 'trusted', 'rotation_required', 'identity_changed'].includes(String(trustState))) throw new Error("Invalid custom-server trust state");
  const parsed: CustomServerAuthorization = {
    authorizationId: safeIdentifier(authorization.authorizationId, "custom-server authorization ID"),
    profileId: safeIdentifier(authorization.profileId, "custom-server profile ID"),
    origin: profileUrl(authorization.origin, "custom-server origin", true),
    discoveryFingerprint: safeIdentifier(authorization.discoveryFingerprint, "custom-server discovery fingerprint"),
    endpointFingerprint: safeIdentifier(authorization.endpointFingerprint, "Companion endpoint fingerprint"),
    trustState: trustState as CustomServerAuthorization["trustState"],
    expiresAt: safeInteger(authorization.expiresAt, 1, "custom-server authorization expiry"),
  };
  if (authorization.previousDiscoveryFingerprint !== undefined) {
    parsed.previousDiscoveryFingerprint = safeIdentifier(authorization.previousDiscoveryFingerprint, "previous custom-server discovery fingerprint");
  }
  const hasPrevious = parsed.previousDiscoveryFingerprint !== undefined;
  if ((parsed.trustState === "first_use") === hasPrevious ||
      (parsed.trustState === "trusted" && parsed.previousDiscoveryFingerprint !== parsed.discoveryFingerprint) ||
      (parsed.trustState === "rotation_required" && parsed.previousDiscoveryFingerprint === parsed.discoveryFingerprint) ||
      !Number.isSafeInteger(receivedAtSeconds) || parsed.expiresAt <= receivedAtSeconds || parsed.expiresAt > receivedAtSeconds + 301) {
    throw new Error("Invalid custom-server trust authorization");
  }
  return parsed;
}

export function parseServerProfiles(value: unknown): ServerProfile[] {
  if (!Array.isArray(value) || value.length > 100) throw new Error("Invalid custom-server profiles");
  const ids = new Set<string>();
  let activeCount = 0;
  return value.map((entry) => {
    const profile = record(entry, "custom-server profile");
    exactKeys(profile, [
      "profileId", "serverUrl", "origin", "deploymentId", "appId", "deviceId", "discoveryFingerprint", "endpointFingerprint", "trustState", "active",
    ], "custom-server profile");
    const profileId = safeIdentifier(profile.profileId, "custom-server profile ID");
    if (ids.has(profileId) || profile.trustState !== "trusted" || typeof profile.active !== "boolean") throw new Error("Invalid custom-server profile");
    ids.add(profileId);
    if (profile.active) activeCount += 1;
    if (activeCount > 1) throw new Error("Invalid active custom-server profile state");
    return {
      profileId,
      serverUrl: profileUrl(profile.serverUrl, "custom-server discovery URL"),
      origin: profileUrl(profile.origin, "custom-server origin", true),
      deploymentId: safeIdentifier(profile.deploymentId, "custom-server deployment ID"),
      appId: safeIdentifier(profile.appId, "custom-server app ID"),
      deviceId: safeIdentifier(profile.deviceId, "custom-server device ID"),
      discoveryFingerprint: safeIdentifier(profile.discoveryFingerprint, "custom-server discovery fingerprint"),
      endpointFingerprint: safeIdentifier(profile.endpointFingerprint, "Companion endpoint fingerprint"),
      trustState: "trusted" as const,
      active: profile.active,
    };
  });
}

function parseProfileConfirmation(value: unknown): { profileId: string; deviceId: string } {
  const result = record(value, "custom-server confirmation");
  exactKeys(result, ["profileId", "deviceId"], "custom-server confirmation");
  return {
    profileId: safeIdentifier(result.profileId, "custom-server profile ID"),
    deviceId: safeIdentifier(result.deviceId, "custom-server device ID"),
  };
}

function parseForgetServerProfile(value: unknown): ForgetServerProfileResult {
  const result = record(value, "forgotten custom-server profile");
  exactKeys(result, ["profileId", "forgotten", "remoteCleanup"], "forgotten custom-server profile");
  if (result.forgotten !== true || (result.remoteCleanup !== "completed" && result.remoteCleanup !== "not_active")) throw new Error("Invalid forgotten custom-server profile result");
  return {
    profileId: safeIdentifier(result.profileId, "forgotten custom-server profile ID"),
    forgotten: true,
    remoteCleanup: result.remoteCleanup,
  };
}

export function parseV2Snapshot(value: unknown): V2CallSnapshotEvent {
  const frame = record(value, "protocol-v2 snapshot");
  exactKeys(frame, ["kind", "schemaVersion", "appId", "sequence", "grants", "snapshot"], "protocol-v2 snapshot");
  if (frame.kind !== "snapshot" || frame.schemaVersion !== 2 || !Array.isArray(frame.grants) || frame.grants.length > 16) {
    throw new Error("Invalid protocol-v2 snapshot");
  }
  const grants = frame.grants.map((grant) => {
    if (typeof grant !== "string" || !V2_GRANTS.has(grant as V2Grant)) throw new Error("Invalid protocol-v2 grant");
    return grant as V2Grant;
  });
  if (new Set(grants).size !== grants.length) throw new Error("Invalid protocol-v2 grants");
  const snapshot = record(frame.snapshot, "protocol-v2 call snapshot");
  exactKeys(snapshot, [
    "callId", "callEpoch", "ownerEpoch", "switchboardRevision", "remoteRevision", "telephonyState", "serviceMode",
    "mediaState", "remoteCapabilities", "secondaryCallPolicy", "secondaryCall", "remoteConsent", "caller", "captions",
    "participants", "audioLevels", "companionMicrophoneMuted", "pendingMobileOffers", "occurredAt",
  ], "protocol-v2 call snapshot");
  const telephony = new Set(["ringing", "active", "held", "ending", "ended"]);
  const service = new Set(["aokie_active", "soft_hold", "consult_pending", "consult_active", "human_pending", "human_active", "returning_to_aokie", "recovering", "ended"]);
  const media = new Set(["none", "ready", "receiving", "connecting", "active", "failed"]);
  if (typeof snapshot.telephonyState !== "string" || !telephony.has(snapshot.telephonyState) ||
      typeof snapshot.serviceMode !== "string" || !service.has(snapshot.serviceMode) ||
      typeof snapshot.mediaState !== "string" || !media.has(snapshot.mediaState)) {
    throw new Error("Invalid protocol-v2 call state");
  }
  const capabilities = record(snapshot.remoteCapabilities, "protocol-v2 remote capabilities");
  exactKeys(capabilities, ["softwareHold", "carrierHoldEvidence", "secondaryCallObservation", "voiceConsult", "takeover"], "protocol-v2 remote capabilities");
  if (typeof capabilities.softwareHold !== "boolean" || typeof capabilities.voiceConsult !== "boolean" || typeof capabilities.takeover !== "boolean" ||
      !["unknown", "negotiated", "observed", "proven"].includes(String(capabilities.carrierHoldEvidence)) ||
      !["unknown", "negotiated", "observed"].includes(String(capabilities.secondaryCallObservation)) ||
      (capabilities.voiceConsult && !capabilities.softwareHold)) {
    throw new Error("Invalid protocol-v2 remote capabilities");
  }
  if (snapshot.secondaryCallPolicy !== "normal" && snapshot.secondaryCallPolicy !== "miss_and_callback") {
    throw new Error("Invalid protocol-v2 secondary call policy");
  }
  if (snapshot.secondaryCall !== undefined && snapshot.secondaryCall !== null) {
    const secondary = record(snapshot.secondaryCall, "protocol-v2 secondary call");
    exactKeys(secondary, ["stable", "callbackEligible", "status", "waitingCallId"], "protocol-v2 secondary call");
    if (capabilities.secondaryCallObservation !== "observed" || typeof secondary.stable !== "boolean" ||
        typeof secondary.callbackEligible !== "boolean" || !["queued", "attempted", "failed"].includes(String(secondary.status)) ||
        (secondary.callbackEligible && snapshot.secondaryCallPolicy !== "miss_and_callback")) {
      throw new Error("Invalid protocol-v2 secondary call");
    }
    if (secondary.waitingCallId !== undefined) {
      safeIdentifier(secondary.waitingCallId, "secondary call ID");
      if (!secondary.stable || !secondary.callbackEligible) throw new Error("Invalid protocol-v2 secondary call identity");
    }
  }
  const consent = record(snapshot.remoteConsent, "protocol-v2 remote consent");
  exactKeys(consent, [
    "policyId", "policyVersion", "enabled", "acknowledged", "acknowledgedAt", "expiresAt",
    "captionsEnabled", "assistanceEnabled", "monitorEnabled", "consultEnabled", "takeoverEnabled",
  ], "protocol-v2 remote consent");
  safeIdentifier(consent.policyId, "remote consent policy ID");
  safeInteger(consent.policyVersion, 1, "remote consent policy version");
  for (const key of ["enabled", "acknowledged", "captionsEnabled", "assistanceEnabled", "monitorEnabled", "consultEnabled", "takeoverEnabled"] as const) {
    if (typeof consent[key] !== "boolean") throw new Error("Invalid protocol-v2 remote consent");
  }
  if (consent.acknowledgedAt !== undefined) timestamp(consent.acknowledgedAt, "remote consent acknowledgement");
  if (consent.expiresAt !== undefined) timestamp(consent.expiresAt, "remote consent expiry");
  if (consent.acknowledged !== (consent.acknowledgedAt !== undefined) ||
      ((!consent.enabled || !consent.acknowledged) && (consent.captionsEnabled || consent.assistanceEnabled || consent.monitorEnabled || consent.consultEnabled || consent.takeoverEnabled))) {
    throw new Error("Invalid protocol-v2 remote consent state");
  }
  if (snapshot.caller !== undefined && snapshot.caller !== null) {
    const caller = record(snapshot.caller, "protocol-v2 caller");
    exactKeys(caller, ["label", "maskedNumber"], "protocol-v2 caller");
    for (const [key, maximum] of [["label", 200], ["maskedNumber", 40]] as const) {
      const entry = caller[key];
      if (entry !== undefined && entry !== null && (typeof entry !== "string" || entry.length > maximum)) throw new Error("Invalid protocol-v2 caller");
    }
  }
  if (snapshot.captions !== undefined) {
    if (!Array.isArray(snapshot.captions) || snapshot.captions.length > 200) throw new Error("Invalid protocol-v2 captions");
    for (const rawCaption of snapshot.captions) {
      const caption = record(rawCaption, "protocol-v2 caption");
      exactKeys(caption, ["captionId", "speaker", "text", "occurredAt", "finalText"], "protocol-v2 caption");
      safeIdentifier(caption.captionId, "caption ID");
      if (typeof caption.speaker !== "string" || caption.speaker.length > 40 || typeof caption.text !== "string" || caption.text.length > 2_000 || typeof caption.finalText !== "boolean") throw new Error("Invalid protocol-v2 caption");
      timestamp(caption.occurredAt, "caption timestamp");
    }
  }
  if (!Array.isArray(snapshot.participants) || snapshot.participants.length > 64) throw new Error("Invalid protocol-v2 participants");
  const participantIds = new Set<string>();
  for (const rawParticipant of snapshot.participants) {
    const participant = record(rawParticipant, "protocol-v2 participant");
    exactKeys(participant, ["participantId", "mode", "state", "subjectId", "displayLabel"], "protocol-v2 participant");
    const participantId = safeIdentifier(participant.participantId, "participant ID");
    if (participantIds.has(participantId) || !["observer", "advisor", "talker"].includes(String(participant.mode)) ||
        !["connected", "prepared", "active"].includes(String(participant.state))) {
      throw new Error("Invalid protocol-v2 participant");
    }
    participantIds.add(participantId);
    if (participant.subjectId !== undefined) safeIdentifier(participant.subjectId, "participant subject ID");
    if (participant.displayLabel !== undefined && (typeof participant.displayLabel !== "string" || !participant.displayLabel || participant.displayLabel.length > 120 || participant.displayLabel.includes("\0"))) {
      throw new Error("Invalid protocol-v2 participant label");
    }
  }
  if (snapshot.audioLevels !== undefined) {
    if (!Array.isArray(snapshot.audioLevels) || snapshot.audioLevels.length > 64) throw new Error("Invalid protocol-v2 audio levels");
    const levelKeys = new Set<string>();
    for (const rawLevel of snapshot.audioLevels) {
      const level = record(rawLevel, "protocol-v2 audio level");
      exactKeys(level, ["source", "participantId", "levelPermille"], "protocol-v2 audio level");
      if (!["caller", "aokie", "companion"].includes(String(level.source)) || !Number.isInteger(level.levelPermille) || Number(level.levelPermille) < 0 || Number(level.levelPermille) > 1_000) {
        throw new Error("Invalid protocol-v2 audio level");
      }
      const participantId = level.participantId === undefined ? "" : safeIdentifier(level.participantId, "audio-level participant ID");
      if (level.source === "companion" && participantId && !participantIds.has(participantId)) {
        throw new Error("Invalid protocol-v2 audio-level source binding");
      }
      const levelKey = `${String(level.source)}:${participantId}`;
      if (levelKeys.has(levelKey)) throw new Error("Duplicate protocol-v2 audio level");
      levelKeys.add(levelKey);
    }
  }
  if (snapshot.companionMicrophoneMuted !== undefined && typeof snapshot.companionMicrophoneMuted !== "boolean") {
    throw new Error("Invalid protocol-v2 microphone mute state");
  }
  if (snapshot.companionMicrophoneMuted === true && snapshot.serviceMode !== "consult_active" && snapshot.serviceMode !== "human_active") {
    throw new Error("Unsafe protocol-v2 microphone mute state");
  }
  if (!Array.isArray(snapshot.pendingMobileOffers) || snapshot.pendingMobileOffers.length > 8) throw new Error("Invalid protocol-v2 mobile offers");
  const offerIds = new Set<string>();
  const offerJtis = new Set<string>();
  for (const rawSignedOffer of snapshot.pendingMobileOffers) {
    const signedOffer = record(rawSignedOffer, "protocol-v2 signed mobile offer");
    exactKeys(signedOffer, ["offer", "offerToken"], "protocol-v2 signed mobile offer");
    if (typeof signedOffer.offerToken !== "string" || !signedOffer.offerToken || signedOffer.offerToken.length > 16 * 1024 || signedOffer.offerToken.includes("\0")) {
      throw new Error("Invalid protocol-v2 mobile offer token");
    }
    const offer = record(signedOffer.offer, "protocol-v2 mobile offer");
    exactKeys(offer, [
      "offerId", "opportunityId", "targetDeviceId", "targetHolderKeyThumbprint", "offeredMode", "surface", "appId", "callId",
      "callEpoch", "ownerEpoch", "switchboardRevision", "remoteRevision", "acceptedTransferRequestId", "requiredConsentPolicyId", "requiredConsentPolicyVersion",
      "requiredGrants", "issuedAt", "expiresAt", "jti",
    ], "protocol-v2 mobile offer");
    const offerId = safeIdentifier(offer.offerId, "mobile offer ID");
    const offerJti = safeIdentifier(offer.jti, "mobile offer JTI");
    safeIdentifier(offer.opportunityId, "mobile offer opportunity");
    safeIdentifier(offer.targetDeviceId, "mobile offer target device");
    safeIdentifier(offer.targetHolderKeyThumbprint, "mobile offer target key");
    safeIdentifier(offer.appId, "mobile offer app");
    safeIdentifier(offer.callId, "mobile offer call");
    safeIdentifier(offer.requiredConsentPolicyId, "mobile offer consent policy");
    if (offerIds.has(offerId) || offerJtis.has(offerJti) || !V2_LEASE_MODES.has(offer.offeredMode as V2LeaseMode) ||
        (offer.surface !== "in_app" && offer.surface !== "voice_system_ui") ||
        (offer.surface === "voice_system_ui" && offer.offeredMode === "monitor")) {
      throw new Error("Invalid protocol-v2 mobile offer");
    }
    offerIds.add(offerId);
    offerJtis.add(offerJti);
    const callEpoch = safeInteger(offer.callEpoch, 1, "mobile offer call epoch");
    const ownerEpoch = safeInteger(offer.ownerEpoch, 0, "mobile offer owner epoch");
    const switchboardRevision = safeInteger(offer.switchboardRevision, 0, "mobile offer switchboard revision");
    const remoteRevision = safeInteger(offer.remoteRevision, 0, "mobile offer remote revision");
    if (offer.acceptedTransferRequestId !== undefined) {
      safeIdentifier(offer.acceptedTransferRequestId, "accepted transfer request ID");
      if (offer.offeredMode !== "takeover") throw new Error("Unsafe protocol-v2 transfer offer");
    }
    const consentVersion = safeInteger(offer.requiredConsentPolicyVersion, 1, "mobile offer consent policy version");
    const issuedAt = safeInteger(offer.issuedAt, 0, "mobile offer issue time");
    const expiresAt = safeInteger(offer.expiresAt, 1, "mobile offer expiry");
    if (expiresAt <= issuedAt || expiresAt - issuedAt > 30 || offer.appId !== frame.appId || offer.callId !== snapshot.callId ||
        callEpoch !== snapshot.callEpoch || ownerEpoch !== snapshot.ownerEpoch || switchboardRevision !== snapshot.switchboardRevision ||
        remoteRevision !== snapshot.remoteRevision || offer.requiredConsentPolicyId !== consent.policyId || consentVersion !== consent.policyVersion) {
      throw new Error("Stale or mismatched protocol-v2 mobile offer");
    }
    if (!Array.isArray(offer.requiredGrants) || offer.requiredGrants.length > 16) throw new Error("Invalid protocol-v2 mobile offer grants");
    const requiredGrants = offer.requiredGrants.map((grant) => {
      if (typeof grant !== "string" || !V2_GRANTS.has(grant as V2Grant)) throw new Error("Invalid protocol-v2 mobile offer grant");
      return grant as V2Grant;
    });
    const modeGrant = offer.offeredMode as V2LeaseMode;
    if (new Set(requiredGrants).size !== requiredGrants.length || !requiredGrants.includes("state_read") || !requiredGrants.includes("rtc_signal") || !requiredGrants.includes(modeGrant) ||
        (offer.acceptedTransferRequestId !== undefined && !requiredGrants.includes("assistance_respond"))) {
      throw new Error("Unsafe protocol-v2 mobile offer grants");
    }
  }
  safeIdentifier(frame.appId, "protocol-v2 app");
  safeInteger(frame.sequence, 1, "protocol-v2 sequence");
  safeIdentifier(snapshot.callId, "protocol-v2 call");
  safeInteger(snapshot.callEpoch, 1, "protocol-v2 call epoch");
  safeInteger(snapshot.ownerEpoch, 0, "protocol-v2 owner epoch");
  safeInteger(snapshot.switchboardRevision, 0, "protocol-v2 switchboard revision");
  safeInteger(snapshot.remoteRevision, 0, "protocol-v2 remote revision");
  timestamp(snapshot.occurredAt, "protocol-v2 timestamp");
  return {
    ...frame,
    snapshot: {
      ...snapshot,
      companionMicrophoneMuted: snapshot.companionMicrophoneMuted === true,
    },
  } as unknown as V2CallSnapshotEvent;
}

export function parseV2IdleSync(value: unknown): V2IdleSyncEvent {
  const frame = record(value, "protocol-v2 idle sync");
  exactKeys(frame, ["kind", "schemaVersion", "appId", "sequence", "grants"], "protocol-v2 idle sync");
  if (frame.kind !== "idle_sync" || frame.schemaVersion !== 2 || !Array.isArray(frame.grants) || frame.grants.length > 16) {
    throw new Error("Invalid protocol-v2 idle sync");
  }
  const grants = frame.grants.map((grant) => {
    if (typeof grant !== "string" || !V2_GRANTS.has(grant as V2Grant)) {
      throw new Error("Invalid protocol-v2 idle sync grant");
    }
    return grant as V2Grant;
  });
  if (!grants.includes("state_read") || new Set(grants).size !== grants.length) {
    throw new Error("Invalid protocol-v2 idle sync grants");
  }
  safeIdentifier(frame.appId, "protocol-v2 idle sync app");
  safeInteger(frame.sequence, 1, "protocol-v2 idle sync sequence");
  return frame as unknown as V2IdleSyncEvent;
}

export function parseV2Assistance(value: unknown): V2AssistanceRequestEvent | null {
  if (value === null) return null;
  const frame = record(value, "protocol-v2 assistance request");
  exactKeys(frame, [
    "kind", "schemaVersion", "appId", "eventId", "requestId", "callId", "callEpoch", "ownerEpoch",
    "switchboardRevision", "remoteRevision", "question", "context", "transferOffered", "expiresAt",
  ], "protocol-v2 assistance request");
  if (frame.kind !== "assistance_request" || frame.schemaVersion !== 2) throw new Error("Invalid protocol-v2 assistance request");
  safeIdentifier(frame.appId, "assistance app ID");
  safeIdentifier(frame.eventId, "assistance event ID");
  safeIdentifier(frame.requestId, "assistance request ID");
  safeIdentifier(frame.callId, "assistance call ID");
  safeInteger(frame.callEpoch, 1, "assistance call epoch");
  safeInteger(frame.ownerEpoch, 0, "assistance owner epoch");
  safeInteger(frame.switchboardRevision, 0, "assistance switchboard revision");
  safeInteger(frame.remoteRevision, 0, "assistance remote revision");
  const expiresAt = safeInteger(frame.expiresAt, 1, "assistance expiry");
  const nowSeconds = Math.floor(Date.now() / 1_000);
  if (expiresAt <= nowSeconds || expiresAt - nowSeconds > 300 || typeof frame.question !== "string" || !frame.question.trim() || frame.question.length > 1_000 || /[\u0000-\u001f\u007f]/.test(frame.question)) {
    throw new Error("Invalid protocol-v2 assistance request");
  }
  if (frame.context !== undefined && (typeof frame.context !== "string" || !frame.context.trim() || frame.context.length > 2_000 || /[\u0000-\u001f\u007f]/.test(frame.context))) {
    throw new Error("Invalid protocol-v2 assistance context");
  }
  if (frame.transferOffered !== undefined && typeof frame.transferOffered !== "boolean") {
    throw new Error("Invalid protocol-v2 transfer offer");
  }
  return { ...frame, transferOffered: frame.transferOffered === true } as unknown as V2AssistanceRequestEvent;
}

export function parseV2AssistanceAnswerAccepted(value: unknown): V2AssistanceAnswerAcceptedEvent {
  const frame = record(value, "protocol-v2 assistance acknowledgement");
  exactKeys(frame, ["kind", "schemaVersion", "appId", "requestId", "answerId", "accepted"], "protocol-v2 assistance acknowledgement");
  if (frame.kind !== "assistance_answer_accepted" || frame.schemaVersion !== 2 || frame.accepted !== true) {
    throw new Error("Invalid protocol-v2 assistance acknowledgement");
  }
  safeIdentifier(frame.appId, "assistance acknowledgement app ID");
  safeIdentifier(frame.requestId, "assistance acknowledgement request ID");
  safeIdentifier(frame.answerId, "assistance acknowledgement answer ID");
  return frame as unknown as V2AssistanceAnswerAcceptedEvent;
}

function validateEndCallerAuthority(frame: Record<string, unknown>, label: string): void {
  safeIdentifier(frame.deviceId, `${label} device ID`);
  safeIdentifier(frame.callId, `${label} call ID`);
  safeInteger(frame.callEpoch, 1, `${label} call epoch`);
  safeInteger(frame.ownerEpoch, 0, `${label} owner epoch`);
  safeInteger(frame.switchboardRevision, 0, `${label} switchboard revision`);
  safeInteger(frame.remoteRevision, 0, `${label} remote revision`);
  safeIdentifier(frame.leaseId, `${label} lease ID`);
  safeInteger(frame.fence, 1, `${label} fence`);
}

export function parseV2EndCaller(value: unknown): V2EndCallerEvent {
  const frame = record(value, "protocol-v2 caller-ending event");
  if (frame.kind === "end_caller_challenge") {
    exactKeys(frame, [
      "kind", "schemaVersion", "appId", "requestId", "confirmationId", "deviceId", "callId",
      "callEpoch", "ownerEpoch", "switchboardRevision", "remoteRevision", "leaseId", "fence", "expiresAt",
    ], "protocol-v2 caller-ending challenge");
    if (frame.schemaVersion !== 2) throw new Error("Invalid protocol-v2 caller-ending challenge");
    safeIdentifier(frame.appId, "caller-ending app ID");
    safeIdentifier(frame.requestId, "caller-ending request ID");
    safeIdentifier(frame.confirmationId, "caller-ending confirmation ID");
    validateEndCallerAuthority(frame, "caller-ending challenge");
    const expiresAt = safeInteger(frame.expiresAt, 1, "caller-ending challenge expiry");
    const nowSeconds = Math.floor(Date.now() / 1_000);
    if (expiresAt <= nowSeconds || expiresAt - nowSeconds > 300) {
      throw new Error("Invalid protocol-v2 caller-ending challenge expiry");
    }
    return frame as unknown as V2EndCallerEvent;
  }
  if (frame.kind === "end_caller_submitted") {
    exactKeys(frame, [
      "kind", "schemaVersion", "appId", "requestId", "operationId", "confirmationId", "accepted",
    ], "protocol-v2 caller-ending submission");
    if (frame.schemaVersion !== 2 || frame.accepted !== true) throw new Error("Invalid protocol-v2 caller-ending submission");
    safeIdentifier(frame.appId, "caller-ending app ID");
    safeIdentifier(frame.requestId, "caller-ending request ID");
    safeIdentifier(frame.operationId, "caller-ending operation ID");
    safeIdentifier(frame.confirmationId, "caller-ending confirmation ID");
    return frame as unknown as V2EndCallerEvent;
  }
  if (frame.kind === "end_caller_result") {
    exactKeys(frame, [
      "kind", "schemaVersion", "appId", "operationId", "confirmationId", "deviceId", "callId",
      "callEpoch", "ownerEpoch", "switchboardRevision", "remoteRevision", "leaseId", "fence",
      "outcome", "code", "message",
    ], "protocol-v2 caller-ending result");
    if (frame.schemaVersion !== 2 || (frame.outcome !== "completed" && frame.outcome !== "failed")) {
      throw new Error("Invalid protocol-v2 caller-ending result");
    }
    safeIdentifier(frame.appId, "caller-ending app ID");
    safeIdentifier(frame.operationId, "caller-ending operation ID");
    safeIdentifier(frame.confirmationId, "caller-ending confirmation ID");
    validateEndCallerAuthority(frame, "caller-ending result");
    if (frame.code !== undefined) safeIdentifier(frame.code, "caller-ending result code");
    if (frame.message !== undefined && (
      typeof frame.message !== "string" || !frame.message.trim() || frame.message.length > 500 || /[\u0000-\u001f\u007f]/.test(frame.message)
    )) throw new Error("Invalid protocol-v2 caller-ending result message");
    return frame as unknown as V2EndCallerEvent;
  }
  if (frame.kind === "end_caller_failure") {
    exactKeys(frame, ["kind", "schemaVersion", "requestId", "code", "message"], "protocol-v2 caller-ending failure");
    if (frame.schemaVersion !== 2) throw new Error("Invalid protocol-v2 caller-ending failure");
    safeIdentifier(frame.requestId, "caller-ending request ID");
    safeIdentifier(frame.code, "caller-ending failure code");
    if (typeof frame.message !== "string" || !frame.message.trim() || frame.message.length > 500 || /[\u0000-\u001f\u007f]/.test(frame.message)) {
      throw new Error("Invalid protocol-v2 caller-ending failure message");
    }
    return frame as unknown as V2EndCallerEvent;
  }
  throw new Error("Invalid protocol-v2 caller-ending event");
}

export function parseV2Lease(value: unknown): V2LeaseEvent | null {
  if (value === null) return null;
  const event = record(value, "protocol-v2 lease");
  exactKeys(event, ["session", "mode", "phase", "provisional"], "protocol-v2 lease");
  if (typeof event.mode !== "string" || !V2_LEASE_MODES.has(event.mode as V2LeaseMode) ||
      (event.phase !== "prepared" && event.phase !== "active") || typeof event.provisional !== "boolean" ||
      event.provisional !== (event.phase === "prepared")) {
    throw new Error("Invalid protocol-v2 lease");
  }
  return {
    session: parseNativeMediaSession(event.session),
    mode: event.mode as V2LeaseMode,
    phase: event.phase,
    provisional: event.provisional,
  };
}

function parseRequestReceipt(value: unknown): V2RequestReceipt {
  const receipt = record(value, "protocol-v2 request receipt");
  exactKeys(receipt, ["requestId"], "protocol-v2 request receipt");
  return { requestId: safeIdentifier(receipt.requestId, "protocol-v2 request ID") };
}

function parseAssistanceReceipt(value: unknown): { requestId: string; answerId: string } {
  const receipt = record(value, "protocol-v2 assistance receipt");
  exactKeys(receipt, ["requestId", "answerId"], "protocol-v2 assistance receipt");
  return {
    requestId: safeIdentifier(receipt.requestId, "assistance request ID"),
    answerId: safeIdentifier(receipt.answerId, "assistance answer ID"),
  };
}

function parseManagedIceServers(value: unknown): NativeIceServer[] {
  if (!Array.isArray(value) || value.length > 16) throw new Error("Invalid managed ICE configuration");
  return value.map((entry) => {
    const server = record(entry, "managed ICE server");
    exactKeys(server, ["urls", "username", "credential"], "managed ICE server");
    if (
      !Array.isArray(server.urls) || server.urls.length === 0 || server.urls.length > 8 ||
      server.urls.some((url) => typeof url !== "string" || url.length > 2_048 || !/^(?:stun|turn|turns):/i.test(url)) ||
      (server.username !== undefined && (typeof server.username !== "string" || server.username.length > 512)) ||
      (server.credential !== undefined && (typeof server.credential !== "string" || server.credential.length > 4_096))
    ) throw new Error("Invalid managed ICE server");
    return {
      urls: server.urls as string[],
      ...(typeof server.username === "string" ? { username: server.username } : {}),
      ...(typeof server.credential === "string" ? { credential: server.credential } : {}),
    };
  });
}

export function parseManagedConnectConfig(value: unknown): RealtimeConfig {
  const config = record(value, "managed connection configuration");
  exactKeys(config, [
    "gatewayUrl", "appId", "deviceId", "accessToken", "protocolVersion", "managedDeploymentId", "managedProfileId", "iceServers", "relayOnly", "localPilot",
  ], "managed connection configuration");
  if (typeof config.gatewayUrl !== "string") throw new Error("Invalid managed gateway URL");
  let gateway: URL;
  try { gateway = new URL(config.gatewayUrl); }
  catch { throw new Error("Invalid managed gateway URL"); }
  const loopback = ["localhost", "127.0.0.1", "[::1]"].includes(gateway.hostname);
  const numericLoopback = ["127.0.0.1", "[::1]"].includes(gateway.hostname);
  if (typeof config.localPilot !== "boolean") throw new Error("Invalid managed local-pilot authority");
  if (
    (gateway.protocol !== "wss:" &&
      !(import.meta.env.DEV && loopback && gateway.protocol === "ws:") &&
      !(config.localPilot && numericLoopback && gateway.protocol === "ws:")) ||
    !gateway.pathname.endsWith("/v2/realtime") || gateway.username || gateway.password || gateway.hash
  ) throw new Error("Invalid managed gateway URL");
  if (config.accessToken !== "" || config.protocolVersion !== 2 || typeof config.relayOnly !== "boolean") {
    throw new Error("Invalid managed connection authority");
  }
  return {
    gatewayUrl: config.gatewayUrl,
    appId: safeIdentifier(config.appId, "managed app ID"),
    deviceId: safeIdentifier(config.deviceId, "managed device ID"),
    accessToken: "",
    protocolVersion: 2,
    managedDeploymentId: safeIdentifier(config.managedDeploymentId, "managed deployment ID"),
    managedProfileId: safeIdentifier(config.managedProfileId, "managed profile ID"),
    iceServers: parseManagedIceServers(config.iceServers),
    relayOnly: config.relayOnly,
    localPilot: config.localPilot,
  };
}

function requiredNullable<T>(
  source: Record<string, unknown>,
  key: string,
  parse: (value: unknown) => T,
  name: string,
): T | null {
  if (!Object.prototype.hasOwnProperty.call(source, key)) throw new Error(`Invalid ${name}`);
  return source[key] === null ? null : parse(source[key]);
}

function companionTimestamp(value: unknown, name: string): string {
  if (typeof value !== "string" || (!DB_TIMESTAMP.test(value) && !RFC3339.test(value))) {
    throw new Error(`Invalid ${name}`);
  }
  const legacyDatabaseUtc = DB_TIMESTAMP.test(value);
  const date = new Date(legacyDatabaseUtc ? `${value.replace(" ", "T")}Z` : value);
  if (!Number.isFinite(date.valueOf()) || (legacyDatabaseUtc
    && date.toISOString().slice(0, 19).replace("T", " ") !== value)) {
    throw new Error(`Invalid ${name}`);
  }
  return date.toISOString().replace(/\.000Z$/, "Z");
}

function companionText(value: unknown, maximum: number, name: string): string {
  if (typeof value !== "string" || !value || value.length > maximum || /[\u0000-\u001f\u007f]/.test(value)) {
    throw new Error(`Invalid ${name}`);
  }
  return value;
}

function companionCallTimestamp(value: unknown, name: string): string {
  return companionTimestamp(value, name);
}

function parseCompanionStaff(value: unknown): CompanionStaffMember[] {
  if (!Array.isArray(value) || value.length > 200) throw new Error("Invalid Companion staff directory");
  const ids = new Set<string>();
  let currentUsers = 0;
  const staff = value.map((rawMember) => {
    const member = record(rawMember, "Companion staff member");
    exactKeys(member, ["id", "displayName", "roleName", "isCurrentUser", "isOwner", "companionReady"], "Companion staff member");
    const id = safeIdentifier(member.id, "Companion staff ID");
    if (ids.has(id) || typeof member.isCurrentUser !== "boolean" || typeof member.isOwner !== "boolean" || typeof member.companionReady !== "boolean") {
      throw new Error("Invalid Companion staff member");
    }
    ids.add(id);
    if (member.isCurrentUser && ++currentUsers > 1) throw new Error("Invalid Companion current staff identity");
    return {
      id,
      displayName: companionText(member.displayName, 120, "Companion staff name"),
      roleName: companionText(member.roleName, 120, "Companion staff role"),
      isCurrentUser: member.isCurrentUser,
      isOwner: member.isOwner,
      companionReady: member.companionReady,
    };
  });
  if (staff.length > 0 && currentUsers !== 1) throw new Error("Invalid Companion current staff identity");
  return staff;
}

function parseCompanionCapabilities(value: unknown): CompanionCapability[] {
  if (!Array.isArray(value) || value.length === 0 || value.length > 11) throw new Error("Invalid Companion capabilities");
  const capabilities = value.map((entry) => {
    if (typeof entry !== "string" || !COMPANION_CAPABILITIES.has(entry as CompanionCapability)) {
      throw new Error("Invalid Companion capability");
    }
    return entry as CompanionCapability;
  });
  if (new Set(capabilities).size !== capabilities.length || !capabilities.includes("state_read")) {
    throw new Error("Invalid Companion capabilities");
  }
  return capabilities;
}

function parseCompanionAvailabilityRecord(value: unknown): CompanionAvailabilityRecord {
  const availability = record(value, "Companion availability record");
  exactKeys(availability, ["availability", "updatedAt", "expiresAt"], "Companion availability record");
  if (typeof availability.availability !== "string" || !COMPANION_AVAILABILITY.has(availability.availability as CompanionAvailabilityState)) {
    throw new Error("Invalid Companion availability");
  }
  return {
    availability: availability.availability as CompanionAvailabilityState,
    updatedAt: companionTimestamp(availability.updatedAt, "Companion availability update"),
    expiresAt: requiredNullable(availability, "expiresAt", (entry) => companionTimestamp(entry, "Companion availability expiry"), "Companion availability expiry"),
  };
}

function parseCompanionRoutingGroups(value: unknown): CompanionRoutingGroup[] {
  if (!Array.isArray(value) || value.length > 200) throw new Error("Invalid Companion routing groups");
  const ids = new Set<string>();
  return value.map((entry) => {
    const group = record(entry, "Companion routing group");
    exactKeys(group, ["id", "name", "policy", "enabled", "members"], "Companion routing group");
    const id = safeIdentifier(group.id, "Companion routing group ID");
    if (ids.has(id) || !["all", "priority", "round_robin"].includes(String(group.policy)) || typeof group.enabled !== "boolean" || !Array.isArray(group.members) || group.members.length > 200) {
      throw new Error("Invalid Companion routing group");
    }
    ids.add(id);
    const members = group.members.map((rawMember) => {
      const member = record(rawMember, "Companion routing member");
      exactKeys(member, [
        "staffId", "displayName", "roleName", "isCurrentUser", "priority", "enabled",
        "availability", "availabilityUpdatedAt", "availabilityExpiresAt",
      ], "Companion routing member");
      if (typeof member.availability !== "string" || !COMPANION_AVAILABILITY.has(member.availability as CompanionAvailabilityState) ||
        typeof member.isCurrentUser !== "boolean" || typeof member.enabled !== "boolean" ||
        !Number.isSafeInteger(member.priority) || Number(member.priority) < 0 || Number(member.priority) > 1_000_000) {
        throw new Error("Invalid Companion routing member availability");
      }
      return {
        staffId: requiredNullable(member, "staffId", (staffId) => safeIdentifier(staffId, "Companion routing staff ID"), "Companion routing staff ID"),
        displayName: companionText(member.displayName, 120, "Companion routing member name"),
        roleName: requiredNullable(member, "roleName", (roleName) => companionText(roleName, 120, "Companion routing member role"), "Companion routing member role"),
        isCurrentUser: member.isCurrentUser,
        priority: Number(member.priority),
        enabled: member.enabled,
        availability: member.availability as CompanionAvailabilityState,
        availabilityUpdatedAt: companionTimestamp(member.availabilityUpdatedAt, "Companion routing member update"),
        availabilityExpiresAt: requiredNullable(member, "availabilityExpiresAt", (expiry) => companionTimestamp(expiry, "Companion routing member expiry"), "Companion routing member expiry"),
      };
    });
    return {
      id,
      name: companionText(group.name, 120, "Companion routing group name"),
      policy: group.policy as CompanionRoutingGroup["policy"],
      enabled: group.enabled,
      members,
    };
  });
}

function validateCompanionRoutingStaffConsistency(
  groups: CompanionRoutingGroup[],
  staff: CompanionStaffMember[],
): void {
  const staffById = new Map(staff.map((member) => [member.id, member]));
  for (const member of groups.flatMap((group) => group.members)) {
    if (member.staffId === null) continue;
    const staffMember = staffById.get(member.staffId);
    // The staff directory is bounded. A routing identity outside this page is
    // valid, but any identity present in both projections must agree exactly.
    if (!staffMember) continue;
    if (member.displayName !== staffMember.displayName || member.roleName !== staffMember.roleName || member.isCurrentUser !== staffMember.isCurrentUser) {
      throw new Error("Invalid Companion routing staff identity");
    }
  }
}

function parseCompanionCallRecord(value: unknown): CompanionCallRecord {
  const item = record(value, "Companion call record");
  exactKeys(item, [
    "id", "callId", "callerName", "maskedNumber", "status", "direction", "summary",
    "startedAt", "endedAt", "durationSeconds", "followUpRequired", "submittedAt",
  ], "Companion call record");
  if (!['inbound', 'outbound', 'unknown'].includes(String(item.direction)) || typeof item.followUpRequired !== "boolean") {
    throw new Error("Invalid Companion call record");
  }
  const maskedNumber = requiredNullable(item, "maskedNumber", (number) => {
    const masked = companionText(number, 40, "Companion masked caller number");
    if ((masked.match(/\d/g) ?? []).length > 4) throw new Error("Companion caller number is not redacted");
    return masked;
  }, "Companion masked caller number");
  const durationSeconds = requiredNullable(item, "durationSeconds", (duration) => {
    if (!Number.isSafeInteger(duration) || Number(duration) < 0) throw new Error("Invalid Companion call duration");
    return Number(duration);
  }, "Companion call duration");
  return {
    id: safeIdentifier(item.id, "Companion call record ID"),
    callId: safeIdentifier(item.callId, "Companion call ID"),
    callerName: requiredNullable(item, "callerName", (name) => companionText(name, 200, "Companion caller name"), "Companion caller name"),
    maskedNumber,
    status: companionText(item.status, 64, "Companion call status"),
    direction: item.direction as CompanionCallRecord["direction"],
    summary: requiredNullable(item, "summary", (summary) => companionText(summary, 5_000, "Companion call summary"), "Companion call summary"),
    startedAt: requiredNullable(item, "startedAt", (timestamp) => companionCallTimestamp(timestamp, "Companion call start"), "Companion call start"),
    endedAt: requiredNullable(item, "endedAt", (timestamp) => companionCallTimestamp(timestamp, "Companion call end"), "Companion call end"),
    durationSeconds,
    followUpRequired: item.followUpRequired,
    submittedAt: companionCallTimestamp(item.submittedAt, "Companion call submission"),
  };
}

export function parseCompanionCallRecords(value: unknown): CompanionCallRecords {
  const response = record(value, "Companion call records");
  exactKeys(response, ["records", "access"], "Companion call records");
  if (!Array.isArray(response.records) || response.records.length > 100 || !["full", "own", "none"].includes(String(response.access))) {
    throw new Error("Invalid Companion call records");
  }
  const records = response.records.map(parseCompanionCallRecord);
  if (new Set(records.map((entry) => entry.id)).size !== records.length) throw new Error("Duplicate Companion call records");
  if (response.access === "none" && records.length > 0) throw new Error("Companion call-record access denied response contains records");
  return { records, access: response.access as CompanionCallRecords["access"] };
}

export function parseCompanionCallRecordDetail(value: unknown, expectedRecordId?: string): CompanionCallRecordDetail {
  const detail = record(value, "Companion call record detail");
  exactKeys(detail, ["record", "transcript", "followUps"], "Companion call record detail");
  if (!Array.isArray(detail.transcript) || detail.transcript.length > 200 || !Array.isArray(detail.followUps) || detail.followUps.length > 100) {
    throw new Error("Invalid Companion call record detail");
  }
  const parsedRecord = parseCompanionCallRecord(detail.record);
  if (expectedRecordId !== undefined && parsedRecord.id !== safeIdentifier(expectedRecordId, "Companion call record ID")) {
    throw new Error("Companion call-record detail does not match the requested record");
  }
  const transcript = detail.transcript.map((rawTurn) => {
    const turn = record(rawTurn, "Companion transcript turn");
    exactKeys(turn, ["id", "speaker", "text", "occurredAt"], "Companion transcript turn");
    return {
      id: safeIdentifier(turn.id, "Companion transcript turn ID"),
      speaker: companionText(turn.speaker, 64, "Companion transcript speaker"),
      text: companionText(turn.text, 10_000, "Companion transcript text"),
      occurredAt: companionCallTimestamp(turn.occurredAt, "Companion transcript timestamp"),
    };
  });
  if (new Set(transcript.map((turn) => turn.id)).size !== transcript.length) {
    throw new Error("Duplicate Companion transcript turns");
  }
  const followUps = detail.followUps.map((rawFollowUp) => {
    const followUp = record(rawFollowUp, "Companion follow-up");
    exactKeys(followUp, ["id", "summary", "status", "priority", "submittedAt"], "Companion follow-up");
    return {
      id: safeIdentifier(followUp.id, "Companion follow-up ID"),
      summary: companionText(followUp.summary, 2_000, "Companion follow-up summary"),
      status: companionText(followUp.status, 64, "Companion follow-up status"),
      priority: companionText(followUp.priority, 64, "Companion follow-up priority"),
      submittedAt: companionCallTimestamp(followUp.submittedAt, "Companion follow-up timestamp"),
    };
  });
  if (new Set(followUps.map((followUp) => followUp.id)).size !== followUps.length) {
    throw new Error("Duplicate Companion follow-ups");
  }
  return { record: parsedRecord, transcript, followUps };
}

export function parseCompanionHistory(value: unknown): CompanionHistory {
  const history = record(value, "Companion history");
  exactKeys(history, ["activity", "sessions"], "Companion history");
  if (!Array.isArray(history.activity) || history.activity.length > 200 || !Array.isArray(history.sessions) || history.sessions.length > 200) {
    throw new Error("Invalid Companion history");
  }
  const activity = history.activity.map((rawActivity) => {
    const item = record(rawActivity, "Companion activity");
    exactKeys(item, [
      "id", "eventId", "appId", "sessionRecordId", "callId", "deviceId", "actorUserId", "subjectId",
      "eventType", "mode", "reason", "ownerEpoch", "occurredAt",
    ], "Companion activity");
    if (typeof item.eventType !== "string" || !COMPANION_ACTIVITY_TYPES.has(item.eventType)) throw new Error("Invalid Companion activity type");
    const mode = requiredNullable(item, "mode", (entry) => {
      if (typeof entry !== "string" || !COMPANION_SESSION_MODES.has(entry)) throw new Error("Invalid Companion activity mode");
      return entry as "monitor" | "consult" | "takeover";
    }, "Companion activity mode");
    const ownerEpoch = requiredNullable(item, "ownerEpoch", (entry) => safeInteger(entry, 0, "Companion activity owner epoch"), "Companion activity owner epoch");
    const optionalId = (key: string) => requiredNullable(item, key, (entry) => safeIdentifier(entry, `Companion activity ${key}`), `Companion activity ${key}`);
    const reason = requiredNullable(item, "reason", (entry) => companionText(entry, 120, "Companion activity reason"), "Companion activity reason");
    return {
      id: safeIdentifier(item.id, "Companion activity ID"),
      eventId: safeIdentifier(item.eventId, "Companion activity event ID"),
      appId: safeIdentifier(item.appId, "Companion activity app ID"),
      sessionRecordId: optionalId("sessionRecordId"),
      callId: optionalId("callId"),
      deviceId: optionalId("deviceId"),
      actorUserId: optionalId("actorUserId"),
      subjectId: safeIdentifier(item.subjectId, "Companion activity subject ID"),
      eventType: item.eventType as CompanionHistory["activity"][number]["eventType"],
      mode,
      reason,
      ownerEpoch,
      occurredAt: companionTimestamp(item.occurredAt, "Companion activity timestamp"),
    };
  });
  const sessions = history.sessions.map((rawSession) => {
    const session = record(rawSession, "Companion session");
    exactKeys(session, [
      "id", "sessionId", "callId", "deviceId", "subjectId", "mode", "state", "joinedAt", "endedAt",
      "endReason", "lastEventId", "lastEventAt",
    ], "Companion session");
    if (typeof session.mode !== "string" || !COMPANION_SESSION_MODES.has(session.mode) || !["prepared", "joined", "left", "revoked"].includes(String(session.state))) {
      throw new Error("Invalid Companion session state");
    }
    return {
      id: safeIdentifier(session.id, "Companion session ID"),
      sessionId: safeIdentifier(session.sessionId, "Companion external session ID"),
      callId: safeIdentifier(session.callId, "Companion session call ID"),
      deviceId: requiredNullable(session, "deviceId", (entry) => safeIdentifier(entry, "Companion session device ID"), "Companion session device ID"),
      subjectId: safeIdentifier(session.subjectId, "Companion session subject ID"),
      mode: session.mode as CompanionHistory["sessions"][number]["mode"],
      state: session.state as CompanionHistory["sessions"][number]["state"],
      joinedAt: requiredNullable(session, "joinedAt", (entry) => companionTimestamp(entry, "Companion session joinedAt"), "Companion session joinedAt"),
      endedAt: requiredNullable(session, "endedAt", (entry) => companionTimestamp(entry, "Companion session endedAt"), "Companion session endedAt"),
      endReason: requiredNullable(session, "endReason", (entry) => companionText(entry, 120, "Companion session end reason"), "Companion session end reason"),
      lastEventId: safeIdentifier(session.lastEventId, "Companion session last event ID"),
      lastEventAt: companionTimestamp(session.lastEventAt, "Companion session last event timestamp"),
    };
  });
  return { activity, sessions };
}

function parsePushEndpoints(value: unknown): CompanionPushEndpoint[] {
  if (!Array.isArray(value) || value.length > 3) throw new Error("Invalid Companion push endpoints");
  return value.map((rawEndpoint) => {
    const endpoint = record(rawEndpoint, "Companion push endpoint");
    exactKeys(endpoint, [
      "id", "appId", "deviceId", "kind", "mode", "provider", "environment", "topic", "fingerprint", "invalidatedAt", "rotatedAt",
    ], "Companion push endpoint");
    if (!["fcm", "apns", "apns_voip"].includes(String(endpoint.kind)) || !["managed", "broker"].includes(String(endpoint.mode)) || !["fcm", "apns", "broker"].includes(String(endpoint.provider)) || !["sandbox", "production"].includes(String(endpoint.environment)) || typeof endpoint.fingerprint !== "string" || !/^[0-9a-f]{64}$/.test(endpoint.fingerprint)) {
      throw new Error("Invalid Companion push endpoint");
    }
    return {
      id: safeIdentifier(endpoint.id, "Companion push endpoint ID"),
      appId: safeIdentifier(endpoint.appId, "Companion push endpoint app ID"),
      deviceId: safeIdentifier(endpoint.deviceId, "Companion push endpoint device ID"),
      kind: endpoint.kind as CompanionPushEndpoint["kind"],
      mode: endpoint.mode as CompanionPushEndpoint["mode"],
      provider: endpoint.provider as CompanionPushEndpoint["provider"],
      environment: endpoint.environment as CompanionPushEndpoint["environment"],
      topic: requiredNullable(endpoint, "topic", (entry) => companionText(entry, 255, "Companion push topic"), "Companion push topic"),
      fingerprint: endpoint.fingerprint,
      invalidatedAt: requiredNullable(endpoint, "invalidatedAt", (entry) => companionTimestamp(entry, "Companion push invalidation"), "Companion push invalidation"),
      rotatedAt: companionTimestamp(endpoint.rotatedAt, "Companion push rotation"),
    };
  });
}

export function parseCompanionBootstrap(value: unknown): CompanionBootstrap {
  const bootstrap = record(value, "Companion bootstrap");
  exactKeys(bootstrap, ["membership", "device", "capabilities", "availability", "routingGroups", "staff", "history", "pushEndpoints"], "Companion bootstrap");
  const membership = record(bootstrap.membership, "Companion membership");
  exactKeys(membership, ["appId", "appSlug", "status"], "Companion membership");
  if (membership.status !== "active") throw new Error("Invalid Companion membership");
  const device = record(bootstrap.device, "Companion device");
  exactKeys(device, ["id", "userId", "appId", "subjectId", "role", "displayName", "grants", "approvedAt", "lastSeenAt", "revokedAt"], "Companion device");
  if (device.role !== "mobile" || !Object.prototype.hasOwnProperty.call(device, "revokedAt") || device.revokedAt !== null) throw new Error("Invalid Companion device");
  const parsedDevice = {
    id: safeIdentifier(device.id, "Companion device ID"),
    userId: safeIdentifier(device.userId, "Companion device user ID"),
    appId: safeIdentifier(device.appId, "Companion device app ID"),
    subjectId: safeIdentifier(device.subjectId, "Companion device subject ID"),
    role: "mobile" as const,
    displayName: companionText(device.displayName, 120, "Companion device name"),
    grants: parseCompanionCapabilities(device.grants),
    approvedAt: companionTimestamp(device.approvedAt, "Companion device approval"),
    lastSeenAt: companionTimestamp(device.lastSeenAt, "Companion device last seen"),
    revokedAt: null,
  };
  const parsedMembership = {
    appId: safeIdentifier(membership.appId, "Companion membership app ID"),
    appSlug: safeIdentifier(membership.appSlug, "Companion membership app slug"),
    status: "active" as const,
  };
  const capabilities = parseCompanionCapabilities(bootstrap.capabilities);
  if (parsedMembership.appId !== parsedDevice.appId || capabilities.some((grant) => !parsedDevice.grants.includes(grant))) {
    throw new Error("Invalid Companion bootstrap binding");
  }
  const pushEndpoints = parsePushEndpoints(bootstrap.pushEndpoints);
  if (pushEndpoints.some((endpoint) => endpoint.appId !== parsedDevice.appId || endpoint.deviceId !== parsedDevice.id)) {
    throw new Error("Invalid Companion push endpoint binding");
  }
  const routingGroups = parseCompanionRoutingGroups(bootstrap.routingGroups);
  const staff = parseCompanionStaff(bootstrap.staff);
  validateCompanionRoutingStaffConsistency(routingGroups, staff);
  return {
    membership: parsedMembership,
    device: parsedDevice,
    capabilities,
    availability: requiredNullable(bootstrap, "availability", parseCompanionAvailabilityRecord, "Companion availability"),
    routingGroups,
    staff,
    history: parseCompanionHistory(bootstrap.history),
    pushEndpoints,
  };
}

export function parseCompanionRouting(value: unknown): CompanionRouting {
  const routing = record(value, "Companion routing");
  exactKeys(routing, ["routingGroups", "staff"], "Companion routing");
  const routingGroups = parseCompanionRoutingGroups(routing.routingGroups);
  const staff = parseCompanionStaff(routing.staff);
  validateCompanionRoutingStaffConsistency(routingGroups, staff);
  return { routingGroups, staff };
}

export function parseCompanionAvailability(value: unknown): CompanionAvailability {
  const response = record(value, "Companion availability");
  exactKeys(response, ["appId", "deviceId", "availability"], "Companion availability");
  return {
    appId: safeIdentifier(response.appId, "Companion availability app ID"),
    deviceId: safeIdentifier(response.deviceId, "Companion availability device ID"),
    availability: requiredNullable(response, "availability", parseCompanionAvailabilityRecord, "Companion availability"),
  };
}

export class TauriCompanionBridge implements CompanionBridge {
  readonly kind = "native" as const;
  private listeners = new Set<Listener>();
  private unlisten: UnlistenFn[] = [];
  private listening: Promise<void> | null = null;

  async getRuntimeCapabilities(): Promise<RuntimeCapabilities> {
    return invoke<RuntimeCapabilities>("runtime_capabilities");
  }

  async discover(url: string): Promise<DiscoveryDocument> {
    return invoke<DiscoveryDocument>("discover_deployment", { url });
  }

  async beginCustomServerAuthorization(serverUrl: string, appId?: string): Promise<CustomServerAuthorization> {
    profileUrl(serverUrl, "custom-server URL");
    if (appId !== undefined) safeIdentifier(appId, "custom-server app ID");
    return parseCustomServerAuthorization(await invoke("native_begin_custom_server_authorization", {
      request: { serverUrl, ...(appId ? { appId } : {}) },
    }));
  }

  async confirmCustomServerTrust(authorization: CustomServerAuthorization, approved: boolean): Promise<{ profileId: string; deviceId: string }> {
    const parsed = parseCustomServerAuthorization(authorization);
    if (typeof approved !== "boolean") throw new Error("Invalid custom-server trust decision");
    return parseProfileConfirmation(await invoke("native_confirm_custom_server_trust", {
      request: {
        authorizationId: parsed.authorizationId,
        discoveryFingerprint: parsed.discoveryFingerprint,
        approved,
      },
    }));
  }

  async connectProfile(profileId: string): Promise<RealtimeConfig> {
    safeIdentifier(profileId, "custom-server profile ID");
    return parseManagedConnectConfig(await invoke("native_connect_profile", { request: { profileId } }));
  }

  async listServerProfiles(): Promise<ServerProfile[]> {
    return parseServerProfiles(await invoke("native_list_server_profiles"));
  }

  async rotateServerTrust(profileId: string): Promise<CustomServerAuthorization> {
    safeIdentifier(profileId, "custom-server profile ID");
    return parseCustomServerAuthorization(await invoke("native_rotate_server_trust", { request: { profileId } }));
  }

  async forgetServerProfile(profileId: string): Promise<ForgetServerProfileResult> {
    safeIdentifier(profileId, "custom-server profile ID");
    return parseForgetServerProfile(await invoke("native_forget_server_profile", { request: { profileId } }));
  }

  async authorizeManaged(discoveryUrl: string, deviceId: string, appId?: string): Promise<RealtimeConfig> {
    return parseManagedConnectConfig(await invoke("managed_authorize", {
      discoveryUrl,
      deviceId,
      ...(appId ? { appId } : {}),
    }));
  }

  async restoreManaged(): Promise<RealtimeConfig | null> {
    const restored = await invoke<unknown>("managed_restore");
    return restored === null ? null : parseManagedConnectConfig(restored);
  }

  async forgetManaged(): Promise<void> {
    await invoke("managed_forget");
  }

  async requestNotificationPermission(): Promise<boolean> {
    return invoke<boolean>("request_notification_permission");
  }

  async getCompanionBootstrap(): Promise<CompanionBootstrap> {
    return parseCompanionBootstrap(await invoke("companion_bootstrap"));
  }

  async getCompanionHistory(limit?: number, before?: number): Promise<CompanionHistory> {
    if (limit !== undefined && (!Number.isInteger(limit) || limit < 1 || limit > 200)) throw new Error("Invalid Companion history limit");
    if (before !== undefined && (!Number.isSafeInteger(before) || before < 1)) throw new Error("Invalid Companion history cursor");
    return parseCompanionHistory(await invoke("companion_history", {
      ...(limit !== undefined ? { limit } : {}),
      ...(before !== undefined ? { before } : {}),
    }));
  }

  async getCompanionRouting(): Promise<CompanionRouting> {
    return parseCompanionRouting(await invoke("companion_routing"));
  }

  async getCompanionCallRecords(limit?: number): Promise<CompanionCallRecords> {
    if (limit !== undefined && (!Number.isInteger(limit) || limit < 1 || limit > 100)) throw new Error("Invalid Companion call-record limit");
    return parseCompanionCallRecords(await invoke("companion_call_records", limit === undefined ? {} : { limit }));
  }

  async getCompanionCallRecordDetail(recordId: string): Promise<CompanionCallRecordDetail> {
    safeIdentifier(recordId, "Companion call record ID");
    return parseCompanionCallRecordDetail(
      await invoke("companion_call_record_detail", { recordId }),
      recordId,
    );
  }

  async getCompanionAvailability(): Promise<CompanionAvailability> {
    return parseCompanionAvailability(await invoke("companion_availability"));
  }

  async setCompanionAvailability(availability: CompanionAvailabilityState, expiresInSeconds?: number): Promise<CompanionAvailability> {
    if (!COMPANION_AVAILABILITY.has(availability) || (expiresInSeconds !== undefined && (!Number.isInteger(expiresInSeconds) || expiresInSeconds < 60 || expiresInSeconds > 86_400))) {
      throw new Error("Invalid Companion availability update");
    }
    return parseCompanionAvailability(await invoke("companion_set_availability", {
      availability,
      ...(expiresInSeconds !== undefined ? { expiresInSeconds } : {}),
    }));
  }

  async connect(config: RealtimeConfig): Promise<void> {
    await this.ensureListeners();
    await invoke("realtime_connect", { config });
  }

  async disconnect(): Promise<void> {
    await invoke("realtime_disconnect");
  }

  async send(command: CommandEnvelope): Promise<void> {
    await invoke("realtime_send", { command });
  }

  async requestV2Lease(mode: V2LeaseMode, acceptedTransferRequestId?: string): Promise<V2RequestReceipt> {
    if (!V2_LEASE_MODES.has(mode)) throw new Error("Unsupported protocol-v2 lease mode");
    if (acceptedTransferRequestId !== undefined) {
      safeIdentifier(acceptedTransferRequestId, "accepted transfer request ID");
      if (mode !== "takeover") throw new Error("Only takeover can accept a transfer request");
    }
    return parseRequestReceipt(await invoke("realtime_v2_request_lease", {
      request: { mode, ...(acceptedTransferRequestId ? { acceptedTransferRequestId } : {}) },
    }));
  }

  async revokeV2Lease(reason: string): Promise<V2RequestReceipt> {
    if (!reason || reason.length > 500 || /[\u0000-\u001f\u007f]/.test(reason)) throw new Error("Invalid protocol-v2 revoke reason");
    return parseRequestReceipt(await invoke("realtime_v2_revoke_lease", { reason }));
  }

  async answerV2Assistance(requestId: string, answer: string, responseAction: "answer" | "decline" = "answer"): Promise<{ requestId: string; answerId: string }> {
    safeIdentifier(requestId, "assistance request ID");
    if (responseAction !== "answer" && responseAction !== "decline") throw new Error("Invalid assistance response action");
    if (!answer.trim() || answer.length > 2_000 || /[\u0000-\u001f\u007f]/.test(answer)) throw new Error("Invalid assistance answer");
    return parseAssistanceReceipt(await invoke("realtime_v2_answer_assistance", {
      request: { requestId, responseAction, answer: answer.trim() },
    }));
  }

  async setV2MicrophoneMuted(muted: boolean): Promise<V2RequestReceipt> {
    if (typeof muted !== "boolean") throw new Error("Invalid microphone mute request");
    return parseRequestReceipt(await invoke("realtime_v2_set_microphone_muted", {
      request: { muted },
    }));
  }

  async prepareEndCaller(): Promise<V2RequestReceipt> {
    await this.ensureListeners();
    return parseRequestReceipt(await invoke("realtime_v2_prepare_end_caller"));
  }

  async confirmEndCaller(confirmationId: string): Promise<V2RequestReceipt> {
    safeIdentifier(confirmationId, "caller-ending confirmation ID");
    return parseRequestReceipt(await invoke("realtime_v2_confirm_end_caller", {
      request: { confirmationId },
    }));
  }

  async confirmDesktopPeerTrust(challenge: DesktopPeerTrustChallenge, approved: boolean): Promise<void> {
    const parsed = parseDesktopPeerTrustChallenge(challenge);
    if (typeof approved !== "boolean") throw new Error("Invalid Desktop peer trust decision");
    await invoke("native_confirm_desktop_peer_trust", {
      request: {
        challengeId: parsed.challengeId,
        profileId: parsed.profileId,
        peerFingerprint: parsed.peerFingerprint,
        approved,
      },
    });
  }

  async reviewDesktopPairingOffer(profileId: string, offerJson: string): Promise<DesktopPairingReview> {
    safeIdentifier(profileId, "Desktop pairing profile ID");
    if (!offerJson.trim() || offerJson.length > MAX_PAIRING_JSON_BYTES) throw new Error("Invalid Desktop pairing offer JSON");
    return parseDesktopPairingReview(await invoke("native_review_desktop_pairing_offer", {
      request: { profileId, offerJson },
    }));
  }

  async confirmDesktopPairing(review: DesktopPairingReview, confirmation: DesktopPairingConfirmation): Promise<DesktopPairingDecision> {
    const parsed = parseDesktopPairingReview(review);
    const { approved, displayName, desktopThumbprintConfirmed, mobileFingerprintAcknowledged } = confirmation;
    if (typeof approved !== "boolean" || typeof desktopThumbprintConfirmed !== "boolean" || typeof mobileFingerprintAcknowledged !== "boolean" ||
        (approved && (!desktopThumbprintConfirmed || !mobileFingerprintAcknowledged || !displayName.trim() || displayName.length > 120 || /[\u0000-\u001f\u007f]/.test(displayName))) ||
        (!approved && (desktopThumbprintConfirmed || mobileFingerprintAcknowledged))) {
      throw new Error("Invalid Desktop pairing confirmation");
    }
    const decision = parseDesktopPairingDecision(await invoke("native_confirm_desktop_pairing", {
      request: {
        reviewId: parsed.reviewId,
        profileId: parsed.profileId,
        desktopKeyThumbprint: parsed.desktopKeyThumbprint,
        desktopFingerprint: parsed.desktopFingerprint,
        mobileKeyThumbprint: parsed.mobileKeyThumbprint,
        mobileFingerprint: parsed.mobileFingerprint,
        displayName: approved ? displayName.trim() : null,
        desktopThumbprintConfirmed,
        mobileFingerprintAcknowledged,
        approved,
      },
    }));
    if (decision.desktopKeyThumbprint !== parsed.desktopKeyThumbprint || decision.mobileKeyThumbprint !== parsed.mobileKeyThumbprint ||
        decision.mobileFingerprint !== parsed.mobileFingerprint || decision.deviceId !== parsed.deviceId || decision.approved !== approved) {
      throw new Error("Desktop pairing decision does not match the reviewed ceremony");
    }
    return decision;
  }

  async getAudioDevices(): Promise<NativeAudioDevices> {
    return parseNativeAudioDevices(await invoke("media_get_audio_devices"));
  }

  async selectAudioDevices(inputId: string, outputId: string): Promise<NativeAudioDevices> {
    audioDeviceId(inputId, "input audio device ID");
    audioDeviceId(outputId, "output audio device ID");
    return parseNativeAudioDevices(await invoke("media_select_audio_devices", {
      request: { inputId, outputId },
    }));
  }

  async createMediaOffer(request: NativeMediaOfferRequest): Promise<NativeMediaOffer> {
    await this.ensureListeners();
    return parseNativeMediaOffer(await invoke("media_create_offer", { request }));
  }

  async acceptMediaAnswer(session: NativeMediaSession, answer: NativeSdpSignal): Promise<void> {
    await invoke("media_accept_answer", { request: { session, answer } });
  }

  async addMediaIceCandidate(session: NativeMediaSession, candidate: NativeIceCandidate): Promise<void> {
    await invoke("media_add_ice_candidate", { request: { session, candidate } });
  }

  async armMediaMicrophone(session: NativeMediaSession): Promise<void> {
    if (session.mode === "monitor" || session.mode === "prepared_talk") throw new Error("Provisional/listen-only media cannot request the microphone.");
    await invoke("media_arm_microphone", { request: { session } });
  }

  async disarmMediaMicrophone(session: NativeMediaSession): Promise<void> {
    await invoke("media_disarm_microphone", { request: { session } });
  }

  async renewMediaLease(session: NativeMediaSession): Promise<void> {
    await invoke("media_renew_lease", { request: { session } });
  }

  async revokeMedia(session: NativeMediaSession, reason?: string): Promise<void> {
    await invoke("media_revoke", { request: { session, ...(reason ? { reason } : {}) } });
  }

  async closeMedia(): Promise<void> {
    await invoke("media_close");
  }

  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    void this.ensureListeners();
    return () => this.listeners.delete(listener);
  }

  async dispose(): Promise<void> {
    await this.listening;
    for (const unlisten of this.unlisten.splice(0)) unlisten();
  }

  private emit(event: BridgeEvent) {
    for (const listener of this.listeners) listener(event);
  }

  private ensureListeners(): Promise<void> {
    if (this.listening) return this.listening;
    this.listening = Promise.all([
      listen<unknown>("aokie-companion://snapshot", ({ payload }) => {
        try {
          this.emit({ type: "snapshot", value: parseCallSnapshot(payload) });
        } catch (error) {
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid snapshot" });
        }
      }),
      listen<unknown>("aokie-companion://sync-ready", ({ payload }) => {
        try {
          this.emit({ type: "sync_ready", value: parseSyncReady(payload) });
        } catch (error) {
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid realtime sync proof" });
        }
      }),
      listen<unknown>("aokie-companion://command-ack", ({ payload }) => {
        try {
          this.emit({ type: "command_ack", value: parseCommandAck(payload) });
        } catch (error) {
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid command acknowledgement" });
        }
      }),
      listen<unknown>("aokie-companion://local-media", ({ payload }) => {
        // The capability grants listen/unlisten only; this proof must originate
        // from the future native media plugin, never from renderer event emit.
        try {
          this.emit({ type: "local_media", value: parseLocalMediaProof(payload) });
        } catch (error) {
          this.emit({ type: "local_media", value: null });
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid local media proof" });
        }
      }),
      listen<unknown>("aokie-companion://media-signal", ({ payload }) => {
        try {
          this.emit({ type: "media_signal", value: parseNativeMediaSignalEvent(payload) });
        } catch (error) {
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid native media signal" });
        }
      }),
      listen<unknown>("aokie-companion://media-state", ({ payload }) => {
        try {
          this.emit({ type: "media_state", value: parseNativeMediaStateEvent(payload) });
        } catch (error) {
          this.emit({ type: "local_media", value: null });
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid native media state" });
        }
      }),
      listen<unknown>("aokie-companion://media-levels", ({ payload }) => {
        try {
          this.emit({ type: "media_levels", value: parseNativeMediaLevelsEvent(payload) });
        } catch (error) {
          this.emit({ type: "media_levels", value: null });
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid native media levels" });
        }
      }),
      listen<unknown>("aokie-companion://v2-snapshot", ({ payload }) => {
        try {
          this.emit({ type: "v2_snapshot", value: parseV2Snapshot(payload) });
        } catch (error) {
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid protocol-v2 snapshot" });
        }
      }),
      listen<unknown>("aokie-companion://v2-idle-sync", ({ payload }) => {
        try {
          this.emit({ type: "v2_idle_sync", value: parseV2IdleSync(payload) });
        } catch (error) {
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid protocol-v2 idle sync" });
        }
      }),
      listen<unknown>("aokie-companion://v2-lease", ({ payload }) => {
        try {
          this.emit({ type: "v2_lease", value: parseV2Lease(payload) });
        } catch (error) {
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid protocol-v2 lease" });
        }
      }),
      listen<unknown>("aokie-companion://v2-assistance", ({ payload }) => {
        try {
          this.emit({ type: "v2_assistance", value: parseV2Assistance(payload) });
        } catch (error) {
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid protocol-v2 assistance request" });
        }
      }),
      listen<unknown>("aokie-companion://v2-assistance-answered", ({ payload }) => {
        try {
          this.emit({ type: "v2_assistance_answered", value: parseV2AssistanceAnswerAccepted(payload) });
        } catch (error) {
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid protocol-v2 assistance acknowledgement" });
        }
      }),
      listen<unknown>("aokie-companion://v2-end-caller", ({ payload }) => {
        try {
          this.emit({ type: "v2_end_caller", value: parseV2EndCaller(payload) });
        } catch (error) {
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid protocol-v2 caller-ending event" });
        }
      }),
      listen<unknown>("aokie-companion://desktop-peer-trust-required", ({ payload }) => {
        try {
          this.emit({ type: "desktop_peer_trust_required", value: parseDesktopPeerTrustChallenge(payload) });
        } catch (error) {
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid Desktop peer trust challenge" });
        }
      }),
      listen<unknown>("aokie-companion://managed-admission-state", ({ payload }) => {
        try {
          this.emit({ type: "managed_admission_state", value: parseManagedAdmissionState(payload) });
        } catch (error) {
          this.emit({ type: "error", message: error instanceof Error ? error.message : "Invalid managed admission state" });
        }
      }),
      listen<BridgeEvent>("aokie-companion://transport", ({ payload }) => {
        if (payload.type === "transport" || payload.type === "error") this.emit(payload);
      }),
    ]).then((unlisten) => {
      this.unlisten.push(...unlisten);
    });
    return this.listening;
  }
}

export function nativeBridgeAvailable(): boolean {
  return isTauri();
}
