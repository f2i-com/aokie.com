import { useCallback, useEffect, useMemo, useReducer, useRef, useState, type ReactNode } from "react";
import {
  AlertTriangle,
  ArrowLeft,
  Bot,
  CheckCircle2,
  Clock3,
  Headphones,
  History,
  LockKeyhole,
  MessageSquareText,
  Mic,
  MicOff,
  PhoneCall,
  PhoneOff,
  Radio,
  RefreshCw,
  Send,
  Server,
  ShieldCheck,
  Sparkles,
  Smartphone,
  Users,
  Volume2,
  Wifi,
  WifiOff,
} from "lucide-react";
import {
  createCompanionBridge,
  type CompanionBridge,
  type CustomServerAuthorization,
  type DesktopPeerTrustChallenge,
  type DiscoveryDocument,
  type LocalMediaProof,
  type ManagedAdmissionState,
  type NativeMediaSession,
  type NativeMediaLevelsEvent,
  type NativeMediaStateEvent,
  type RealtimeConfig,
  type RuntimeCapabilities,
  type ServerProfile,
  type V2CallSnapshotEvent,
  type V2AssistanceAnswerAcceptedEvent,
  type V2AssistanceRequestEvent,
  type V2EndCallerChallengeEvent,
  type V2EndCallerEvent,
  type V2IdleSyncEvent,
  type V2LeaseEvent,
  type V2Grant,
  type V2LeaseMode,
  type V2PendingMobileOffer,
} from "./bridge";
import AokieCompanion from "./features/shell/AokieCompanion";
import V2WorkspaceShell, { type CompanionAvailabilityValue } from "./features/live/V2WorkspaceShell";
import DesktopPairingDialog from "./features/pairing/DesktopPairingDialog";
import { createCommand, MAX_ASSISTANCE_ANSWER_CHARACTERS } from "./protocol/codec";
import type { CallSnapshot, CommandEnvelope, CommandPayloads, CommandType } from "./protocol/types";
import { shouldClearAssistanceDraft } from "./state/assistanceDraft";
import {
  areMutatingControlsLocked,
  canMonitor,
  canTakeOver,
  companionCallReducer,
  createCompanionCallState,
} from "./state/callReducer";
import { displayError } from "./utils/displayError";

type AppMode = "setup" | "demo" | "live";
type SendCommand = <Type extends CommandType>(
  type: Type,
  payload: CommandPayloads[Type],
) => Promise<boolean>;

interface CommandWaiter {
  timer: number;
  resolve(accepted: boolean): void;
}

const DEFAULT_RUNTIME: RuntimeCapabilities = {
  platform: "web",
  tauri: false,
  secureStorage: false,
  notifications: false,
  microphone: false,
  nativeCallUi: false,
  realtime: false,
  mediaBridge: false,
  demo: false,
  localPilot: false,
  notificationPermission: "not_applicable",
  microphonePermission: "unknown",
  fcmConfigured: false,
  fcmTokenPresent: false,
  pushRegistration: "not_applicable",
  pendingCallOffer: false,
  batteryOptimizationsRestricted: false,
  forceStopState: "not_detectable",
  callInfrastructure: "not_applicable",
  lastNativeDiagnostic: null,
};
const DEFAULT_DISCOVERY_URL = import.meta.env.VITE_AOKIE_DISCOVERY_URL?.trim()
  || (import.meta.env.DEV
    ? "http://api.formlogic.local/.well-known/aokie-companion"
    : "https://formlogic.example/api/app/my-business/aokie-discovery");
const DEFAULT_APP_ID = import.meta.env.VITE_AOKIE_APP_ID?.trim()
  || (import.meta.env.DEV ? "app_local" : "app_development");

export function desktopPairingOpenAfterAdmission(
  currentlyOpen: boolean,
  admissionValue: ManagedAdmissionState["value"],
): boolean {
  return currentlyOpen || admissionValue === "pairing_required";
}

export function shouldAcceptV2AuthoritativeSequence(
  lastSequence: number,
  incomingSequence: number,
): boolean {
  return Number.isSafeInteger(lastSequence)
    && lastSequence >= 0
    && Number.isSafeInteger(incomingSequence)
    && incomingSequence > lastSequence;
}

export function assistanceExpiryDelayMs(expiresAtSeconds: number, nowMs = Date.now()): number {
  if (!Number.isSafeInteger(expiresAtSeconds) || expiresAtSeconds <= 0 || !Number.isFinite(nowMs)) return 0;
  return Math.max(0, Math.min(expiresAtSeconds * 1_000 - nowMs, 2_147_000_000));
}

export function formatAssistanceCountdown(remainingSeconds: number): string {
  const bounded = Number.isFinite(remainingSeconds) ? Math.max(0, Math.floor(remainingSeconds)) : 0;
  const minutes = Math.floor(bounded / 60);
  const seconds = bounded % 60;
  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

const V2_OFFER_CLOCK_SKEW_SECONDS = 5;
const ARMABLE_MEDIA_PHASES = ["connected", "remote_audio_ready", "microphone_armed", "microphone_disarmed", "answer_applied"];
const CONNECTED_MEDIA_PHASES = ["connected", "remote_audio_ready", "microphone_armed", "microphone_disarmed"];
const NATIVE_MEDIA_LEVEL_TTL_MS = 1_000;

export type V2RuntimeFailureOperation = "realtime" | "request" | "takeover" | "microphone" | "end_caller";

export interface V2RuntimeFailure {
  message: string;
  operation: V2RuntimeFailureOperation;
  occurredAt: number;
}

export type V2RuntimeFailureAction =
  | { type: "report"; failure: V2RuntimeFailure }
  | { type: "dismiss" }
  | { type: "begin_takeover" }
  | { type: "authoritative_idle" };

export function v2RuntimeFailureReducer(
  state: V2RuntimeFailure | null,
  action: V2RuntimeFailureAction,
): V2RuntimeFailure | null {
  if (action.type === "report") return action.failure;
  if (action.type === "authoritative_idle") return state?.operation === "realtime" ? null : state;
  return null;
}

export type TakeoverConfirmationMode = "press_and_hold" | "two_step";

export function takeoverConfirmationMode(platform: string): TakeoverConfirmationMode {
  return ["windows", "win32"].includes(platform.trim().toLowerCase()) ? "two_step" : "press_and_hold";
}

export function v2LeaseExitLabel(mode: V2LeaseMode): string {
  if (mode === "monitor") return "Stop listening";
  if (mode === "consult") return "Finish private consult";
  return "Return to Aokie";
}

export function v2LeaseRevokeAllowed(connected: boolean, lease: V2LeaseEvent | null): boolean {
  return connected && lease !== null;
}

export interface V2CurrentAccessPolicyPresentation {
  title: string;
  detail: string;
}

export function v2CurrentAccessPolicyPresentation(
  snapshot: V2CallSnapshotEvent | null,
  currentConsent: boolean,
): V2CurrentAccessPolicyPresentation | null {
  if (!snapshot) return null;
  const consent = snapshot.snapshot.remoteConsent;
  if (!currentConsent || !consent.enabled || !consent.acknowledged) {
    return {
      title: "Current remote access is not acknowledged",
      detail: "The currently published disclosure and acknowledgement do not permit listening, private consultation, or takeover. Live controls remain locked until newer authoritative call policy is published.",
    };
  }

  const grants = new Set(snapshot.grants);
  if (!grants.has("rtc_signal")) {
    return {
      title: "Current live-media authority is limited",
      detail: "This device's current FormLogic grants do not include live-media signalling, so listening, private consultation, and takeover remain locked under the current policy.",
    };
  }

  const consentPermitsAnyMode = consent.monitorEnabled || consent.consultEnabled || consent.takeoverEnabled;
  const monitorAuthorized = consent.monitorEnabled && grants.has("monitor");
  const consultAuthorized = consent.consultEnabled && grants.has("consult");
  const takeoverAuthorized = consent.takeoverEnabled && grants.has("takeover") && grants.has("resume_aokie");
  if (!monitorAuthorized && !consultAuthorized && !takeoverAuthorized) {
    return consentPermitsAnyMode
      ? {
          title: "Current device policy permits no live actions",
          detail: "This device's current FormLogic grants do not authorize listening, private consultation, or takeover under the published call policy. Controls remain locked until newer authoritative access is published.",
        }
      : {
          title: "Current remote policy permits no live actions",
          detail: "The currently published call policy does not permit listening, private consultation, or takeover. Controls remain locked until newer authoritative policy is published.",
        };
  }
  return null;
}

export interface ConfirmedTakeoverTarget {
  appId: string;
  callId: string;
  callEpoch: number;
  ownerEpoch: number;
  switchboardRevision: number;
  remoteRevision: number;
  targetDeviceId: string;
  requiredConsentPolicyId: string;
  requiredConsentPolicyVersion: number;
  requiredGrants: V2Grant[];
}

export interface V2TakeoverAttemptState {
  generation: number;
  target: ConfirmedTakeoverTarget | null;
  requestPending: boolean;
  autoArmRequiresConnected: boolean;
  autoArmRetryCount: number;
  autoArmRetryAfterMediaRevision: number | null;
}

export type V2TakeoverAttemptAction =
  | { type: "begin" }
  | { type: "target_confirmed"; generation: number; target: ConfirmedTakeoverTarget }
  | { type: "request_enqueued"; generation: number }
  | { type: "lease_published" }
  | { type: "failed_or_idle" }
  | { type: "auto_arm_started" }
  | { type: "auto_arm_retryable"; generation: number; target: ConfirmedTakeoverTarget; armStartMediaRevision: number }
  | { type: "workspace_navigation" };

export const INITIAL_V2_TAKEOVER_ATTEMPT: V2TakeoverAttemptState = {
  generation: 0,
  target: null,
  requestPending: false,
  autoArmRequiresConnected: false,
  autoArmRetryCount: 0,
  autoArmRetryAfterMediaRevision: null,
};

export function v2TakeoverAttemptReducer(
  state: V2TakeoverAttemptState,
  action: V2TakeoverAttemptAction,
): V2TakeoverAttemptState {
  if (action.type === "workspace_navigation") return state;
  if (action.type === "begin") {
    return { ...INITIAL_V2_TAKEOVER_ATTEMPT, generation: state.generation + 1 };
  }
  if (action.type === "target_confirmed") {
    return action.generation === state.generation ? { ...state, target: action.target } : state;
  }
  if (action.type === "request_enqueued") {
    return action.generation === state.generation ? { ...state, requestPending: true } : state;
  }
  if (action.type === "lease_published") {
    if (!state.target && !state.requestPending) return state;
    return { ...state, generation: state.generation + 1, requestPending: false };
  }
  if (action.type === "auto_arm_started") {
    return {
      ...state,
      target: null,
      requestPending: false,
      autoArmRequiresConnected: false,
      autoArmRetryAfterMediaRevision: null,
    };
  }
  if (action.type === "auto_arm_retryable") {
    if (
      action.generation !== state.generation || state.target || state.autoArmRetryCount >= 1 ||
      !Number.isSafeInteger(action.armStartMediaRevision) || action.armStartMediaRevision < 0
    ) return state;
    return {
      ...state,
      target: action.target,
      requestPending: false,
      autoArmRequiresConnected: true,
      autoArmRetryCount: state.autoArmRetryCount + 1,
      autoArmRetryAfterMediaRevision: action.armStartMediaRevision,
    };
  }
  return { ...INITIAL_V2_TAKEOVER_ATTEMPT, generation: state.generation + 1 };
}

function confirmedTakeoverTargetsEqual(
  left: ConfirmedTakeoverTarget | null,
  right: ConfirmedTakeoverTarget | null,
): boolean {
  return left === right || Boolean(
    left && right &&
    left.appId === right.appId &&
    left.callId === right.callId &&
    left.callEpoch === right.callEpoch &&
    left.ownerEpoch === right.ownerEpoch &&
    left.switchboardRevision === right.switchboardRevision &&
    left.remoteRevision === right.remoteRevision &&
    left.targetDeviceId === right.targetDeviceId &&
    left.requiredConsentPolicyId === right.requiredConsentPolicyId &&
    left.requiredConsentPolicyVersion === right.requiredConsentPolicyVersion &&
    left.requiredGrants.length === right.requiredGrants.length &&
    left.requiredGrants.every((grant) => right.requiredGrants.includes(grant)),
  );
}

/**
 * Re-check the mutable attempt immediately before an effect consumes the
 * rendered confirmation. Native events can advance the ref between render and
 * effect; only the same generation and exact call fence may still authorize an
 * automatic microphone arm.
 */
export function currentV2AutoArmAttempt(
  rendered: V2TakeoverAttemptState,
  current: V2TakeoverAttemptState,
): V2TakeoverAttemptState | null {
  return current.generation === rendered.generation
    && confirmedTakeoverTargetsEqual(current.target, rendered.target)
    ? current
    : null;
}

/**
 * React state may still describe the preceding authoritative snapshot when an
 * effect is about to run. Only the exact latest accepted sequence is allowed
 * to authorize a microphone transition.
 */
export function currentV2AutoArmSnapshot(
  rendered: V2CallSnapshotEvent | null,
  current: V2CallSnapshotEvent | null,
): V2CallSnapshotEvent | null {
  return rendered && current && rendered.appId === current.appId
    && rendered.sequence === current.sequence
    ? current
    : null;
}

function consentExpiryIsCurrent(expiresAt: string | undefined, nowMilliseconds: number): boolean {
  if (!expiresAt) return true;
  const parsed = Date.parse(expiresAt);
  return Number.isFinite(parsed) && parsed > nowMilliseconds;
}

export function v2RemoteConsentCurrent(
  connected: boolean,
  consent: V2CallSnapshotEvent["snapshot"]["remoteConsent"] | undefined,
  nowMilliseconds = Date.now(),
): boolean {
  return Boolean(
    connected
      && consent?.enabled
      && consent.acknowledged
      && consentExpiryIsCurrent(consent.expiresAt, nowMilliseconds),
  );
}

/**
 * Keeps the local confirmation bound to the exact remote-access policy and
 * admission authority that were current when the operator confirmed it.
 * Claim processing may advance owner/switchboard/remote revisions, but may
 * never weaken or replace those consent and grant facts.
 */
export function v2SnapshotMaintainsConfirmedTakeoverAuthority(
  target: ConfirmedTakeoverTarget | null,
  snapshot: V2CallSnapshotEvent | null,
  nowMilliseconds = Date.now(),
): boolean {
  if (!target || !snapshot) return false;
  const call = snapshot.snapshot;
  const grants = new Set(snapshot.grants);
  return snapshot.appId === target.appId
    && call.callId === target.callId
    && call.callEpoch === target.callEpoch
    && call.ownerEpoch >= target.ownerEpoch
    && call.switchboardRevision >= target.switchboardRevision
    && call.remoteRevision >= target.remoteRevision
    && call.telephonyState === "active"
    && !["returning_to_aokie", "recovering", "ended"].includes(call.serviceMode)
    && call.mediaState !== "none"
    && call.mediaState !== "failed"
    && call.remoteCapabilities.takeover
    && call.remoteConsent.enabled
    && call.remoteConsent.acknowledged
    && call.remoteConsent.takeoverEnabled
    && call.remoteConsent.policyId === target.requiredConsentPolicyId
    && call.remoteConsent.policyVersion === target.requiredConsentPolicyVersion
    && consentExpiryIsCurrent(call.remoteConsent.expiresAt, nowMilliseconds)
    && grants.has("state_read")
    && grants.has("rtc_signal")
    && grants.has("takeover")
    && grants.has("resume_aokie")
    && target.requiredGrants.every((grant) => grants.has(grant));
}

export function v2SnapshotAuthorizesConfirmedTakeoverArm(
  target: ConfirmedTakeoverTarget | null,
  snapshot: V2CallSnapshotEvent | null,
  nowMilliseconds = Date.now(),
): boolean {
  if (!target || !snapshot || !v2SnapshotMaintainsConfirmedTakeoverAuthority(target, snapshot, nowMilliseconds)) {
    return false;
  }
  return snapshot.snapshot.ownerEpoch > target.ownerEpoch
    && ["aokie_active", "human_pending", "human_active"].includes(snapshot.snapshot.serviceMode);
}

function confirmedTakeoverMatchesLease(
  target: ConfirmedTakeoverTarget | null,
  lease: V2LeaseEvent | null,
): boolean {
  return Boolean(
    target && lease &&
    lease.session.appId === target.appId &&
    lease.session.callId === target.callId &&
    lease.session.callEpoch === target.callEpoch &&
    lease.session.deviceId === target.targetDeviceId &&
    // A successful claim advances ownership. Equality describes the
    // pre-claim/prepared fence and must never authorize microphone arming.
    lease.session.ownerEpoch > target.ownerEpoch,
  );
}

export function shouldAutoArmConfirmedTakeover(
  target: ConfirmedTakeoverTarget | null,
  lease: V2LeaseEvent | null,
  nativeMediaState: NativeMediaStateEvent | null,
  exactActiveTakeoverSession: boolean,
  alreadyAttempted: boolean,
  requiresConnectedPhase = false,
  nativeMediaRevision = 0,
  retryAfterMediaRevision: number | null = null,
): boolean {
  return Boolean(
    !alreadyAttempted && exactActiveTakeoverSession &&
    confirmedTakeoverMatchesLease(target, lease) &&
    lease?.mode === "takeover" && lease.phase === "active" && lease.session.mode === "talk" &&
    nativeMediaState?.remoteAudioReady && !nativeMediaState.microphoneActive &&
    ARMABLE_MEDIA_PHASES.includes(nativeMediaState.phase) &&
    (!requiresConnectedPhase || CONNECTED_MEDIA_PHASES.includes(nativeMediaState.phase)) &&
    (retryAfterMediaRevision === null || nativeMediaRevision > retryAfterMediaRevision),
  );
}

export function isRetryableMediaArmFailure(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error ?? "");
  return /WebRTC is still connecting|before WebRTC is connected|microphone authority is still connecting|authority channel is not open/i.test(message);
}

function nativeMediaSessionAttemptKey(session: NativeMediaSession): string {
  return [
    session.appId,
    session.callId,
    session.callEpoch,
    session.ownerEpoch,
    session.leaseId,
    session.fence,
    session.rtcSessionId,
    session.sdpRevision,
    session.transportGeneration,
  ].join(":");
}

export function isExactCurrentV2NativeMediaSession(
  lease: V2LeaseEvent | null,
  nativeMediaState: { session: NativeMediaSession } | null,
  nowMilliseconds = Date.now(),
): boolean {
  if (!lease || !nativeMediaState) return false;
  const native = nativeMediaState.session;
  const expected = lease.session;
  return native.appId === expected.appId
    && native.streamNonce === expected.streamNonce
    && native.rtcSessionId === expected.rtcSessionId
    && native.callId === expected.callId
    && native.callEpoch === expected.callEpoch
    && native.ownerEpoch === expected.ownerEpoch
    && native.deviceId === expected.deviceId
    && native.mode === expected.mode
    && native.leaseId === expected.leaseId
    && native.fence === expected.fence
    && native.sdpRevision === expected.sdpRevision
    && native.transportGeneration === expected.transportGeneration
    && Number.isFinite(Date.parse(expected.expiresAt))
    && Date.parse(expected.expiresAt) > nowMilliseconds;
}

export function isExpectedPreparedTakeoverReplacement(
  target: ConfirmedTakeoverTarget | null,
  lease: V2LeaseEvent | null,
  nativeMediaState: NativeMediaStateEvent,
  nowMilliseconds = Date.now(),
): boolean {
  return Boolean(
    target && lease &&
    nativeMediaState.phase === "replaced" &&
    nativeMediaState.session.mode === "prepared_talk" &&
    lease.mode === "takeover" &&
    lease.phase === "prepared" &&
    lease.provisional &&
    lease.session.ownerEpoch === target.ownerEpoch &&
    lease.session.appId === target.appId &&
    lease.session.callId === target.callId &&
    lease.session.callEpoch === target.callEpoch &&
    isExactCurrentV2NativeMediaSession(lease, nativeMediaState, nowMilliseconds),
  );
}

export function currentV2InAppOffer(
  snapshot: V2CallSnapshotEvent | null,
  deviceId: string,
  mode: V2LeaseMode,
  nowSeconds: number,
  acceptedTransferRequestId?: string,
): V2PendingMobileOffer | null {
  if (!snapshot || !deviceId || !Number.isSafeInteger(nowSeconds) || nowSeconds < 0) return null;
  const call = snapshot.snapshot;
  const grants = new Set(snapshot.grants);
  const consentAllowsMode = call.remoteConsent.enabled
    && call.remoteConsent.acknowledged
    && (mode === "monitor"
      ? call.remoteConsent.monitorEnabled
      : mode === "consult"
        ? call.remoteConsent.consultEnabled
        : call.remoteConsent.takeoverEnabled);
  if (!consentAllowsMode || !grants.has(mode) || !grants.has("rtc_signal")) return null;

  const matches = call.pendingMobileOffers.filter(({ offer }) => (
    offer.surface === "in_app"
    && offer.offeredMode === mode
    && offer.targetDeviceId === deviceId
    && offer.appId === snapshot.appId
    && offer.callId === call.callId
    && offer.callEpoch === call.callEpoch
    && offer.ownerEpoch === call.ownerEpoch
    && offer.switchboardRevision === call.switchboardRevision
    && offer.remoteRevision === call.remoteRevision
    && offer.acceptedTransferRequestId === acceptedTransferRequestId
    && offer.requiredConsentPolicyId === call.remoteConsent.policyId
    && offer.requiredConsentPolicyVersion === call.remoteConsent.policyVersion
    && offer.requiredGrants.every((grant) => grants.has(grant))
    && offer.issuedAt <= nowSeconds + V2_OFFER_CLOCK_SKEW_SECONDS
    && offer.expiresAt > nowSeconds
  ));
  // Native selection rejects ambiguity as well. Keep the renderer locked unless
  // it can identify the same single foreground offer before opening a control.
  return matches.length === 1 ? matches[0] : null;
}

function App() {
  const bridge = useMemo<CompanionBridge>(() => createCompanionBridge(), []);
  const [mode, setMode] = useState<AppMode>("setup");
  const [runtime, setRuntime] = useState(DEFAULT_RUNTIME);
  const [callState, callDispatch] = useReducer(companionCallReducer, undefined, createCompanionCallState);
  const [deviceId, setDeviceId] = useState("device_local_development");
  const [localMediaProof, setLocalMediaProof] = useState<LocalMediaProof | null>(null);
  const [nativeMediaState, setNativeMediaState] = useState<NativeMediaStateEvent | null>(null);
  const [nativeMediaLevels, setNativeMediaLevels] = useState<NativeMediaLevelsEvent | null>(null);
  const [protocolVersion, setProtocolVersion] = useState<1 | 2>(1);
  const protocolVersionRef = useRef<1 | 2>(1);
  const [v2Snapshot, setV2Snapshot] = useState<V2CallSnapshotEvent | null>(null);
  const v2SnapshotRef = useRef<V2CallSnapshotEvent | null>(null);
  const [v2IdleSync, setV2IdleSync] = useState<V2IdleSyncEvent | null>(null);
  const [v2Lease, setV2Lease] = useState<V2LeaseEvent | null>(null);
  const v2LeaseRef = useRef<V2LeaseEvent | null>(null);
  const [v2Assistance, setV2Assistance] = useState<V2AssistanceRequestEvent | null>(null);
  const [v2AssistanceAccepted, setV2AssistanceAccepted] = useState<V2AssistanceAnswerAcceptedEvent | null>(null);
  const [v2EndCaller, setV2EndCaller] = useState<V2EndCallerEvent | null>(null);
  const [desktopPeerTrust, setDesktopPeerTrust] = useState<DesktopPeerTrustChallenge | null>(null);
  const [managedAdmission, setManagedAdmission] = useState<ManagedAdmissionState | null>(null);
  const [desktopPairingOpen, setDesktopPairingOpen] = useState(false);
  const [v2Transport, setV2Transport] = useState<"idle" | "connecting" | "connected" | "reconnecting" | "offline">("idle");
  const [v2Failure, v2FailureDispatch] = useReducer(v2RuntimeFailureReducer, null);
  const [v2TakeoverAttempt, setV2TakeoverAttempt] = useState(INITIAL_V2_TAKEOVER_ATTEMPT);
  const [v2AutoArmInFlight, setV2AutoArmInFlight] = useState(false);
  const v2TakeoverAttemptRef = useRef(INITIAL_V2_TAKEOVER_ATTEMPT);
  const v2AutoArmAuthorityTargetRef = useRef<ConfirmedTakeoverTarget | null>(null);
  const v2AutoArmAttemptedSessions = useRef(new Set<string>());
  const [v2NativeMediaRevision, setV2NativeMediaRevision] = useState(0);
  const v2NativeMediaRevisionRef = useRef(0);
  const [connecting, setConnecting] = useState(false);
  const connectInFlight = useRef(false);
  const currentConnectConfig = useRef<RealtimeConfig | null>(null);
  const peerTrustRetryConfig = useRef<RealtimeConfig | null>(null);
  const restoreAttempted = useRef(false);
  const v2CallId = useRef<string | null>(null);
  const v2LastSequence = useRef(0);
  const mutationSubmissionLocked = useRef(false);
  const commandWaiters = useRef(new Map<string, CommandWaiter>());
  const transitionV2TakeoverAttempt = useCallback((action: V2TakeoverAttemptAction) => {
    const current = v2TakeoverAttemptRef.current;
    const next = v2TakeoverAttemptReducer(current, action);
    if (action.type === "begin" || action.type === "failed_or_idle") {
      v2AutoArmAuthorityTargetRef.current = null;
    } else if (action.type === "target_confirmed" && next !== current) {
      v2AutoArmAuthorityTargetRef.current = action.target;
    }
    v2TakeoverAttemptRef.current = next;
    setV2TakeoverAttempt(next);
    return next;
  }, []);
  const reportV2Failure = useCallback((message: string, operation: V2RuntimeFailureOperation) => {
    transitionV2TakeoverAttempt({ type: "failed_or_idle" });
    v2AutoArmAttemptedSessions.current.clear();
    setV2AutoArmInFlight(false);
    v2FailureDispatch({
      type: "report",
      failure: { message, operation, occurredAt: Date.now() },
    });
  }, [transitionV2TakeoverAttempt]);

  useEffect(() => {
    const unsubscribe = bridge.subscribe((event) => {
      if (event.type === "snapshot") {
        callDispatch({ type: "snapshot", value: event.value });
        if (!event.value.gatewayReachable) setLocalMediaProof(null);
      }
      else if (event.type === "sync_ready") {
        setLocalMediaProof(null);
        setNativeMediaState(null);
        setNativeMediaLevels(null);
        callDispatch({ type: "sync_ready", value: event.value });
      }
      else if (event.type === "local_media") setLocalMediaProof(event.value);
      else if (event.type === "media_state") {
        const nextMediaRevision = v2NativeMediaRevisionRef.current + 1;
        v2NativeMediaRevisionRef.current = nextMediaRevision;
        setV2NativeMediaRevision(nextMediaRevision);
        setNativeMediaState(event.value);
        const terminalMedia = ["closed", "expired", "failed", "revoked", "replaced"].includes(event.value.phase);
        const expectedTakeoverUpgrade = isExpectedPreparedTakeoverReplacement(
          v2TakeoverAttemptRef.current.target,
          v2LeaseRef.current,
          event.value,
        );
        if (!event.value.microphoneActive || terminalMedia) {
          setLocalMediaProof(null);
        }
        if (terminalMedia) setNativeMediaLevels(null);
        if (terminalMedia && !expectedTakeoverUpgrade && protocolVersionRef.current === 2) {
          transitionV2TakeoverAttempt({ type: "failed_or_idle" });
          v2AutoArmAttemptedSessions.current.clear();
          setV2AutoArmInFlight(false);
        }
      }
      else if (event.type === "media_levels") setNativeMediaLevels(event.value);
      else if (event.type === "v2_snapshot") {
        if (!shouldAcceptV2AuthoritativeSequence(v2LastSequence.current, event.value.sequence)) return;
        v2LastSequence.current = event.value.sequence;
        // Publish the authority ref before scheduling React state. A pending
        // effect from the prior render must observe this newer sequence and
        // refuse to arm from stale consent or grant facts.
        v2SnapshotRef.current = event.value;
        const nextCallId = event.value.snapshot.callId;
        if (v2CallId.current && v2CallId.current !== nextCallId) {
          transitionV2TakeoverAttempt({ type: "failed_or_idle" });
          v2AutoArmAttemptedSessions.current.clear();
          setV2AutoArmInFlight(false);
          v2LeaseRef.current = null;
          setV2Lease(null);
          setV2Assistance(null);
          setV2AssistanceAccepted(null);
          setV2EndCaller(null);
          setLocalMediaProof(null);
          setNativeMediaState(null);
          setNativeMediaLevels(null);
        } else if (
          v2AutoArmAuthorityTargetRef.current &&
          !v2SnapshotMaintainsConfirmedTakeoverAuthority(
            v2AutoArmAuthorityTargetRef.current,
            event.value,
          )
        ) {
          // Consent, policy, grants, capability, or a safe call route changed
          // while the claim was settling. The old confirmation is spent.
          transitionV2TakeoverAttempt({ type: "failed_or_idle" });
          v2AutoArmAttemptedSessions.current.clear();
          setV2AutoArmInFlight(false);
        }
        v2CallId.current = nextCallId;
        setV2IdleSync(null);
        setV2Snapshot(event.value);
      }
      else if (event.type === "v2_idle_sync") {
        if (!shouldAcceptV2AuthoritativeSequence(v2LastSequence.current, event.value.sequence)) return;
        v2LastSequence.current = event.value.sequence;
        v2CallId.current = null;
        v2SnapshotRef.current = null;
        setV2Snapshot(null);
        setV2IdleSync(event.value);
        v2LeaseRef.current = null;
        setV2Lease(null);
        setV2Assistance(null);
        setV2AssistanceAccepted(null);
        setV2EndCaller(null);
        setLocalMediaProof(null);
        setNativeMediaState(null);
        setNativeMediaLevels(null);
        transitionV2TakeoverAttempt({ type: "failed_or_idle" });
        v2AutoArmAttemptedSessions.current.clear();
        setV2AutoArmInFlight(false);
        v2FailureDispatch({ type: "authoritative_idle" });
      }
      else if (event.type === "v2_lease") {
        v2LeaseRef.current = event.value;
        setV2Lease(event.value);
        if (event.value?.mode === "takeover" && (event.value.provisional || event.value.phase === "prepared" || event.value.phase === "active")) {
          transitionV2TakeoverAttempt({ type: "lease_published" });
        } else {
          // A reset or replacement is terminal for the consent that selected
          // the prior offer. Never carry it into a later lease on this call.
          transitionV2TakeoverAttempt({ type: "failed_or_idle" });
          v2AutoArmAttemptedSessions.current.clear();
          setV2AutoArmInFlight(false);
        }
      }
      else if (event.type === "v2_assistance") {
        if (event.value) setV2AssistanceAccepted(null);
        setV2Assistance(event.value);
      }
      else if (event.type === "v2_assistance_answered") setV2AssistanceAccepted(event.value);
      else if (event.type === "v2_end_caller") setV2EndCaller(event.value);
      else if (event.type === "desktop_peer_trust_required") {
        peerTrustRetryConfig.current = currentConnectConfig.current;
        setDesktopPeerTrust(event.value);
        connectInFlight.current = false;
        setConnecting(false);
        setMode("setup");
      }
      else if (event.type === "managed_admission_state") {
        setManagedAdmission(event.value);
        setDesktopPairingOpen((currentlyOpen) => desktopPairingOpenAfterAdmission(
          currentlyOpen,
          event.value.value,
        ));
        connectInFlight.current = false;
        setConnecting(false);
        setMode("setup");
      }
      // Media signals are intentionally not interpreted as call authority by
      // React. A versioned gateway transport adapter forwards these exact
      // native events; snapshots and leases remain the UI authority.
      else if (event.type === "media_signal") {
        if (!isCurrent(event.value.session.expiresAt)) setLocalMediaProof(null);
      }
      else if (event.type === "command_ack") {
        callDispatch({ type: "command_ack", value: event.value });
        const waiter = commandWaiters.current.get(event.value.commandId);
        if (waiter) {
          window.clearTimeout(waiter.timer);
          commandWaiters.current.delete(event.value.commandId);
          waiter.resolve(event.value.accepted);
        }
      } else if (event.type === "transport") {
        setV2Transport(event.value);
        callDispatch({ type: "transport", value: event.value });
        if (event.value === "connected") {
          connectInFlight.current = false;
          setConnecting(false);
          setManagedAdmission(null);
          setDesktopPairingOpen(false);
          setMode("live");
        } else if (event.value === "offline" && connectInFlight.current) {
          connectInFlight.current = false;
          setConnecting(false);
        }
        if (event.value !== "connected") {
          transitionV2TakeoverAttempt({ type: "failed_or_idle" });
          v2AutoArmAttemptedSessions.current.clear();
          setV2AutoArmInFlight(false);
          v2LastSequence.current = 0;
          v2SnapshotRef.current = null;
          setLocalMediaProof(null);
          setNativeMediaState(null);
          v2LeaseRef.current = null;
          setV2Lease(null);
          setV2Assistance(null);
          setV2AssistanceAccepted(null);
          setV2EndCaller(null);
          setV2IdleSync(null);
          for (const waiter of commandWaiters.current.values()) {
            window.clearTimeout(waiter.timer);
            waiter.resolve(false);
          }
          commandWaiters.current.clear();
        }
      }
      else if (protocolVersionRef.current === 2) reportV2Failure(event.message, "realtime");
      else callDispatch({ type: "error", message: event.message });
    });
    void bridge.getRuntimeCapabilities().then(setRuntime).catch((error) => {
      callDispatch({ type: "error", message: displayError(error, "Native runtime unavailable") });
    });
    return () => {
      unsubscribe();
      for (const waiter of commandWaiters.current.values()) {
        window.clearTimeout(waiter.timer);
        waiter.resolve(false);
      }
      commandWaiters.current.clear();
    };
  }, [bridge, reportV2Failure, transitionV2TakeoverAttempt]);

  useEffect(() => {
    if (!localMediaProof) return;
    const remaining = Date.parse(localMediaProof.expiresAt) - Date.now();
    if (remaining <= 0) {
      setLocalMediaProof(null);
      return;
    }
    const timer = window.setTimeout(() => {
      setLocalMediaProof((current) => current === localMediaProof ? null : current);
    }, Math.min(remaining + 25, 2_147_000_000));
    return () => window.clearTimeout(timer);
  }, [localMediaProof]);

  useEffect(() => {
    if (!nativeMediaLevels) return;
    const remaining = Date.parse(nativeMediaLevels.measuredAt) + NATIVE_MEDIA_LEVEL_TTL_MS - Date.now();
    if (remaining <= 0) {
      setNativeMediaLevels(null);
      return;
    }
    const timer = window.setTimeout(() => {
      setNativeMediaLevels((current) => current === nativeMediaLevels ? null : current);
    }, remaining + 25);
    return () => window.clearTimeout(timer);
  }, [nativeMediaLevels]);

  useEffect(() => {
    if (!v2Assistance) return;
    const requestId = v2Assistance.requestId;
    const remaining = assistanceExpiryDelayMs(v2Assistance.expiresAt);
    if (remaining <= 0) {
      setV2Assistance((current) => current?.requestId === requestId ? null : current);
      return;
    }
    const timer = window.setTimeout(() => {
      setV2Assistance((current) => current?.requestId === requestId ? null : current);
    }, remaining + 25);
    return () => window.clearTimeout(timer);
  }, [v2Assistance]);

  useEffect(() => {
    // The subscription mutates its ref synchronously, while React may still be
    // about to run an effect from the preceding render. Re-fence that rendered
    // attempt before reading any consent from it.
    const fencedAttempt = currentV2AutoArmAttempt(
      v2TakeoverAttempt,
      v2TakeoverAttemptRef.current,
    );
    const target = fencedAttempt?.target ?? null;
    const lease = v2Lease;
    const media = nativeMediaState;
    const snapshot = currentV2AutoArmSnapshot(v2Snapshot, v2SnapshotRef.current);
    if (!fencedAttempt || !target || !lease || !media || !snapshot) return;

    const call = snapshot.snapshot;
    const exactNativeSession = isExactCurrentV2NativeMediaSession(lease, media);
    const exactActiveTakeoverSession = Boolean(
      v2Transport === "connected" && lease.mode === "takeover" && lease.phase === "active" &&
      v2SnapshotAuthorizesConfirmedTakeoverArm(target, snapshot) &&
      exactNativeSession && snapshot.appId === lease.session.appId &&
      call.callId === lease.session.callId && call.callEpoch === lease.session.callEpoch &&
      call.ownerEpoch === lease.session.ownerEpoch &&
      call.telephonyState === "active",
    );
    const confirmedSessionIsActive = confirmedTakeoverMatchesLease(target, lease)
      && exactActiveTakeoverSession;
    if (confirmedSessionIsActive && media.microphoneActive) {
      transitionV2TakeoverAttempt({ type: "auto_arm_started" });
      return;
    }

    const attemptKey = nativeMediaSessionAttemptKey(media.session);
    if (!shouldAutoArmConfirmedTakeover(
      target,
      lease,
      media,
      exactActiveTakeoverSession,
      v2AutoArmAttemptedSessions.current.has(attemptKey),
      fencedAttempt.autoArmRequiresConnected,
      v2NativeMediaRevision,
      fencedAttempt.autoArmRetryAfterMediaRevision,
    )) return;

    // Consume consent and record the exact fenced session before calling the
    // native bridge. Only a retryable pre-connect failure below may restore
    // this exact target, and then only behind the connected-phase gate.
    const armGeneration = fencedAttempt.generation;
    const armStartMediaRevision = v2NativeMediaRevision;
    v2AutoArmAttemptedSessions.current.add(attemptKey);
    transitionV2TakeoverAttempt({ type: "auto_arm_started" });
    setV2AutoArmInFlight(true);
    void bridge.armMediaMicrophone(media.session)
      .catch((caught) => {
        if (v2TakeoverAttemptRef.current.generation !== armGeneration) return;
        v2AutoArmAttemptedSessions.current.delete(attemptKey);
        if (isRetryableMediaArmFailure(caught) && fencedAttempt.autoArmRetryCount < 1) {
          transitionV2TakeoverAttempt({
            type: "auto_arm_retryable",
            generation: armGeneration,
            target,
            // The rendered revision was current when the first arm began and
            // cannot prove recovery. A later exact connected transition may
            // prove recovery even when it races ahead of this rejection.
            armStartMediaRevision,
          });
          return;
        }
        reportV2Failure(
          displayError(caught, "Takeover was accepted, but the microphone could not be armed. You can retry with Unmute microphone."),
          "microphone",
        );
      })
      .finally(() => {
        if (v2TakeoverAttemptRef.current.generation === armGeneration) setV2AutoArmInFlight(false);
      });
  }, [bridge, nativeMediaState, reportV2Failure, transitionV2TakeoverAttempt, v2Lease, v2NativeMediaRevision, v2Snapshot, v2TakeoverAttempt, v2Transport]);

  const connect = useCallback(async (config: RealtimeConfig) => {
    if (connectInFlight.current) return;
    connectInFlight.current = true;
    currentConnectConfig.current = config;
    setConnecting(true);
    for (const waiter of commandWaiters.current.values()) {
      window.clearTimeout(waiter.timer);
      waiter.resolve(false);
    }
    commandWaiters.current.clear();
    mutationSubmissionLocked.current = false;
    transitionV2TakeoverAttempt({ type: "failed_or_idle" });
    v2AutoArmAttemptedSessions.current.clear();
    setV2AutoArmInFlight(false);
    setDeviceId(config.deviceId);
    setLocalMediaProof(null);
    setNativeMediaState(null);
    setNativeMediaLevels(null);
    const selectedProtocol = config.protocolVersion ?? (config.gatewayUrl.includes("/v2/realtime") ? 2 : 1);
    protocolVersionRef.current = selectedProtocol;
    setProtocolVersion(selectedProtocol);
    v2LastSequence.current = 0;
    v2SnapshotRef.current = null;
    setV2Snapshot(null);
    setV2IdleSync(null);
    v2CallId.current = null;
    v2LeaseRef.current = null;
    setV2Lease(null);
    setV2Assistance(null);
    setV2AssistanceAccepted(null);
    setV2EndCaller(null);
    setV2Transport("connecting");
    callDispatch({ type: "reset" });
    callDispatch({ type: "transport", value: "connecting" });
    try {
      await bridge.connect(config);
    } catch (error) {
      callDispatch({ type: "transport", value: "offline" });
      callDispatch({
        type: "error",
        message: displayError(error, "The realtime connection could not be started"),
      });
      await bridge.disconnect().catch(() => undefined);
      connectInFlight.current = false;
      setConnecting(false);
    }
  }, [bridge, transitionV2TakeoverAttempt]);

  const disconnectToSetup = useCallback(async () => {
    try {
      await bridge.disconnect();
    } finally {
      await bridge.closeMedia().catch(() => undefined);
      for (const waiter of commandWaiters.current.values()) {
        window.clearTimeout(waiter.timer);
        waiter.resolve(false);
      }
      commandWaiters.current.clear();
      mutationSubmissionLocked.current = false;
      transitionV2TakeoverAttempt({ type: "failed_or_idle" });
      v2AutoArmAttemptedSessions.current.clear();
      setV2AutoArmInFlight(false);
      v2LastSequence.current = 0;
      v2SnapshotRef.current = null;
      setV2Snapshot(null);
      setV2IdleSync(null);
      v2CallId.current = null;
      v2LeaseRef.current = null;
      setV2Lease(null);
      setV2Assistance(null);
      setV2AssistanceAccepted(null);
      setV2EndCaller(null);
      setV2Transport("idle");
      setLocalMediaProof(null);
      setNativeMediaState(null);
      setNativeMediaLevels(null);
      callDispatch({ type: "reset" });
      setMode("setup");
      setDesktopPeerTrust(null);
      setManagedAdmission(null);
      setDesktopPairingOpen(false);
      currentConnectConfig.current = null;
      peerTrustRetryConfig.current = null;
    }
  }, [bridge, transitionV2TakeoverAttempt]);

  const forgetActiveServerProfile = useCallback(async () => {
    const profiles = await bridge.listServerProfiles();
    const activeProfile = profiles.find((profile) => profile.active);
    if (activeProfile) {
      await bridge.forgetServerProfile(activeProfile.profileId);
    } else {
      // Retain compatibility with debug/legacy sessions that predate server profiles.
      await bridge.forgetManaged();
    }
    await disconnectToSetup();
  }, [bridge, disconnectToSetup]);

  const loadCompanionBootstrap = useCallback(() => bridge.getCompanionBootstrap(), [bridge]);
  const loadCompanionHistory = useCallback((limit?: number, before?: number) => bridge.getCompanionHistory(limit, before), [bridge]);
  const loadCompanionRouting = useCallback(() => bridge.getCompanionRouting(), [bridge]);
  const loadCompanionCallRecords = useCallback((limit?: number) => bridge.getCompanionCallRecords(limit), [bridge]);
  const loadCompanionCallRecordDetail = useCallback((recordId: string) => bridge.getCompanionCallRecordDetail(recordId), [bridge]);
  const setCompanionAvailability = useCallback(async (value: CompanionAvailabilityValue, expiresInSeconds?: number) => {
    const response = await bridge.setCompanionAvailability(value, expiresInSeconds);
    return response.availability;
  }, [bridge]);

  const refreshRuntime = useCallback(async () => {
    const capabilities = await bridge.getRuntimeCapabilities();
    setRuntime(capabilities);
    return capabilities;
  }, [bridge]);

  useEffect(() => {
    if (restoreAttempted.current || !runtime.tauri || !runtime.secureStorage || mode !== "setup") return;
    restoreAttempted.current = true;
    void bridge.restoreManaged().then(async (restored) => {
      if (restored) await connect(restored);
    }).catch((error) => {
      callDispatch({
        type: "error",
        message: displayError(error, "Saved managed sign-in could not be restored"),
      });
    });
  }, [bridge, connect, mode, runtime.secureStorage, runtime.tauri]);

  const sendCommand = useCallback(async <Type extends CommandType,>(
    type: Type,
    payload: CommandPayloads[Type],
  ) => {
    if (!callState.snapshot || callState.transport !== "connected" || !callState.snapshot.gatewayReachable) {
      callDispatch({ type: "error", message: "Call controls are locked until a fresh live connection is confirmed." });
      return false;
    }
    let command: CommandEnvelope;
    try {
      command = createCommand(callState.snapshot, deviceId, type, payload);
    } catch (error) {
      callDispatch({
        type: "error",
        message: displayError(error, "The command could not be validated"),
      });
      return false;
    }
    if (mutationSubmissionLocked.current || areMutatingControlsLocked(callState)) {
      callDispatch({
        type: "error",
        message: "Another command is pending or awaiting authoritative reconciliation. All call controls remain locked.",
      });
      return false;
    }
    mutationSubmissionLocked.current = true;
    callDispatch({
      type: "command_sent",
      value: {
        commandId: command.commandId,
        streamNonce: callState.snapshot.streamNonce,
        sequence: callState.snapshot.sequence,
      },
    });
    const acknowledgement = new Promise<boolean>((resolve) => {
      const timer = window.setTimeout(() => {
        commandWaiters.current.delete(command.commandId);
        callDispatch({ type: "command_uncertain", commandId: command.commandId });
        resolve(false);
      }, 10_000);
      commandWaiters.current.set(command.commandId, { timer, resolve });
    });
    try {
      await bridge.send(command);
    } catch (error) {
      const waiter = commandWaiters.current.get(command.commandId);
      if (waiter) {
        window.clearTimeout(waiter.timer);
        commandWaiters.current.delete(command.commandId);
        waiter.resolve(false);
      }
      callDispatch({ type: "command_uncertain", commandId: command.commandId });
      callDispatch({
        type: "error",
        message: displayError(error, "Command delivery could not be confirmed."),
      });
    }
    return acknowledgement;
  }, [bridge, callState, deviceId]);

  useEffect(() => {
    mutationSubmissionLocked.current = areMutatingControlsLocked(callState);
  }, [callState]);

  const retryCurrentConnection = useCallback(async () => {
    const retry = currentConnectConfig.current;
    if (!retry) {
      callDispatch({ type: "error", message: "No native connection is available to retry. Select the saved server profile again." });
      return;
    }
    setDesktopPairingOpen(false);
    setManagedAdmission(null);
    await connect(retry);
  }, [connect]);

  const withDeviceDialogs = (content: ReactNode) => (
    <>
      {content}
      {desktopPairingOpen && (
        <DesktopPairingDialog
          bridge={bridge}
          admission={managedAdmission}
          retrying={connecting}
          onClose={() => setDesktopPairingOpen(false)}
          onRetry={retryCurrentConnection}
        />
      )}
      {desktopPeerTrust && (
        <DesktopPeerTrustDialog
          challenge={desktopPeerTrust}
          onDecision={async (approved) => {
            const challenge = desktopPeerTrust;
            await bridge.confirmDesktopPeerTrust(challenge, approved);
            setDesktopPeerTrust(null);
            const retry = peerTrustRetryConfig.current;
            peerTrustRetryConfig.current = null;
            if (approved && retry) await connect(retry);
          }}
          onExpired={() => {
            setDesktopPeerTrust(null);
            peerTrustRetryConfig.current = null;
          }}
        />
      )}
    </>
  );

  if (mode === "demo") {
    return withDeviceDialogs(
      <div className="companion-demo-shell">
        <div className="demo-safety-ribbon">
          <button className="leave-demo" onClick={() => setMode("setup")}><ArrowLeft size={16} /> Exit demo</button>
          <span><AlertTriangle size={16} /> Interactive demo — no phone, microphone, or live gateway is connected</span>
        </div>
        <AokieCompanion />
      </div>
    );
  }

  if (mode === "live") {
    if (protocolVersion === 2) {
      return withDeviceDialogs(
        <div className="v2-workspace-frame">
          {v2Failure && <V2FailureNotice failure={v2Failure} onDismiss={() => v2FailureDispatch({ type: "dismiss" })} />}
          {v2AssistanceAccepted && <V2AssistanceAcceptedNotice onDismiss={() => setV2AssistanceAccepted(null)} />}
          {v2TakeoverAttempt.requestPending && <V2TakeoverPendingNotice />}
          <V2WorkspaceShell
            runtime={runtime}
            transport={v2Transport}
            snapshot={v2Snapshot}
            idleSync={v2IdleSync}
            assistance={v2Assistance}
            loadBootstrap={loadCompanionBootstrap}
            loadHistory={loadCompanionHistory}
            loadRouting={loadCompanionRouting}
            loadCallRecords={loadCompanionCallRecords}
            loadCallRecordDetail={loadCompanionCallRecordDetail}
            loadAudioDevices={() => bridge.getAudioDevices()}
            loadServerProfiles={() => bridge.listServerProfiles()}
            selectAudioDevices={(inputId, outputId) => bridge.selectAudioDevices(inputId, outputId)}
            setAvailability={setCompanionAvailability}
            onDisconnect={disconnectToSetup}
            onForgetManaged={forgetActiveServerProfile}
            onRunSetup={disconnectToSetup}
            renderLiveCall={(onHome, audioEndpointControls, onCalls, callRecordId) => (
              <V2LiveRuntime
                runtime={runtime}
                deviceId={deviceId}
                transport={v2Transport}
                snapshot={v2Snapshot}
                lease={v2Lease}
                assistance={v2Assistance}
                endCaller={v2EndCaller}
                nativeMediaState={nativeMediaState}
                nativeMediaLevels={nativeMediaLevels}
                localMediaProof={localMediaProof}
                autoArmInFlight={v2AutoArmInFlight}
                audioEndpointControls={audioEndpointControls}
                onOpenCalls={onCalls}
                callRecordId={callRecordId}
                onRequestLease={(leaseMode, acceptedTransferRequestId) => bridge.requestV2Lease(leaseMode, acceptedTransferRequestId)}
                onRevokeLease={(reason) => {
                  // A local Return/Stop action immediately consumes any saved
                  // automatic-arm consent. Do not leave a retry window open
                  // while its relay acknowledgement is in flight.
                  transitionV2TakeoverAttempt({ type: "failed_or_idle" });
                  v2AutoArmAttemptedSessions.current.clear();
                  setV2AutoArmInFlight(false);
                  return bridge.revokeV2Lease(reason);
                }}
                onAnswerAssistance={(requestId, answer, responseAction) => bridge.answerV2Assistance(requestId, answer, responseAction)}
                onPrepareEndCaller={async () => {
                  setV2EndCaller(null);
                  return bridge.prepareEndCaller();
                }}
                onConfirmEndCaller={(confirmationId) => bridge.confirmEndCaller(confirmationId)}
                onSetMicrophoneMuted={(muted) => bridge.setV2MicrophoneMuted(muted)}
                onReportFailure={reportV2Failure}
                onBeginTakeover={() => {
                  const attempt = transitionV2TakeoverAttempt({ type: "begin" });
                  v2FailureDispatch({ type: "begin_takeover" });
                  return attempt.generation;
                }}
                onConfirmTakeoverTarget={(generation, target) => transitionV2TakeoverAttempt({ type: "target_confirmed", generation, target })}
                onTakeoverEnqueued={(generation) => transitionV2TakeoverAttempt({ type: "request_enqueued", generation })}
                onBack={async () => {
                  transitionV2TakeoverAttempt({ type: "workspace_navigation" });
                  onHome();
                }}
              />
            )}
          />
        </div>
      );
    }
    return withDeviceDialogs(
      <LiveRuntime
        runtime={runtime}
        state={callState}
        deviceId={deviceId}
        localMediaProof={localMediaProof}
        nativeMediaState={nativeMediaState}
        onCommand={sendCommand}
        onToggleMediaMicrophone={async (session, active) => {
          try {
            if (active) await bridge.disarmMediaMicrophone(session);
            else await bridge.armMediaMicrophone(session);
          } catch (error) {
            callDispatch({
              type: "error",
              message: displayError(error, "Native microphone state could not be changed"),
            });
          }
        }}
        onBack={disconnectToSetup}
      />
    );
  }

  return withDeviceDialogs(
      <SetupScreen
        bridge={bridge}
        runtime={runtime}
        error={callState.lastError}
        connecting={connecting}
        onDemo={() => setMode("demo")}
        onConnect={connect}
        onRefreshRuntime={refreshRuntime}
        onClearError={() => callDispatch({ type: "clear_error" })}
        managedAdmission={managedAdmission}
        onOpenDesktopPairing={() => setDesktopPairingOpen(true)}
        onRetryConnection={retryCurrentConnection}
      />
  );
}

function DesktopPeerTrustDialog({ challenge, onDecision, onExpired }: {
  challenge: DesktopPeerTrustChallenge;
  onDecision(approved: boolean): Promise<void>;
  onExpired(): void;
}) {
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1_000));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const remaining = Math.max(0, challenge.expiresAt - now);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Math.floor(Date.now() / 1_000)), 1_000);
    return () => window.clearInterval(timer);
  }, [challenge.challengeId]);
  const decide = async (approved: boolean) => {
    if (busy || remaining <= 0) return;
    setBusy(true);
    setError(null);
    try { await onDecision(approved); }
    catch (caught) {
      setError(displayError(caught, "The Desktop identity decision could not be saved"));
      setBusy(false);
    }
  };
  return (
    <div className="modal-layer is-centered desktop-peer-trust-modal" role="presentation">
      <section className="modal-sheet is-dialog" role="dialog" aria-modal="true" aria-labelledby="desktop-peer-trust-title">
        <div className={challenge.rotation ? "danger-modal-icon" : "takeover-hero-icon"}>{challenge.rotation ? <AlertTriangle size={24} /> : <LockKeyhole size={24} />}</div>
        <span className="section-kicker">OWNER-CONFIRMED DEVICE TRUST</span>
        <h2 id="desktop-peer-trust-title">{challenge.rotation ? "Aokie Desktop identity changed" : "Trust this Aokie Desktop?"}</h2>
        <p className="modal-lead">Compare the complete fingerprint below with the one shown by the local FormLogic Desktop pairing screen or its owner-confirmed QR. The server cannot approve this key for you.</p>
        {challenge.rotation && challenge.previousPeerFingerprint && <div className="peer-fingerprint is-previous"><span>Previously trusted</span><code>{challenge.previousPeerFingerprint}</code></div>}
        <div className="peer-fingerprint"><span>{challenge.rotation ? "New Desktop fingerprint" : "Desktop fingerprint"}</span><code>{challenge.peerFingerprint}</code></div>
        <div className="end-caller-safety-list"><span><ShieldCheck size={16} /> App {challenge.appId}</span><span><Smartphone size={16} /> This installation {challenge.deviceId}</span><span><RefreshCw size={16} /> Confirmation expires in {remaining}s</span></div>
        {remaining <= 0 && <div className="setup-result is-failed" role="alert"><AlertTriangle size={17} /><span>This comparison expired. Close it and start the connection again to receive a fresh challenge.</span></div>}
        {error && <div className="setup-result is-failed" role="alert"><AlertTriangle size={17} /><span>{error}</span></div>}
        <button className="primary-button" disabled={busy || remaining <= 0} onClick={() => void decide(true)}><ShieldCheck size={18} />{busy ? "Saving exact fingerprint…" : challenge.rotation ? "Approve this key rotation" : "Approve this exact Desktop"}</button>
        <button className="text-button" disabled={busy} onClick={() => remaining <= 0 ? onExpired() : void decide(false)}>{remaining <= 0 ? "Close expired comparison" : "Reject and stay disconnected"}</button>
      </section>
    </div>
  );
}

interface SetupProps {
  bridge: CompanionBridge;
  runtime: RuntimeCapabilities;
  error: string | null;
  connecting: boolean;
  onDemo(): void;
  onConnect(config: RealtimeConfig): Promise<void>;
  onRefreshRuntime(): Promise<RuntimeCapabilities>;
  onClearError(): void;
  managedAdmission: ManagedAdmissionState | null;
  onOpenDesktopPairing(): void;
  onRetryConnection(): Promise<void>;
}

function SetupScreen({ bridge, runtime, error, connecting, onDemo, onConnect, onRefreshRuntime, onClearError, managedAdmission, onOpenDesktopPairing, onRetryConnection }: SetupProps) {
  const [discoveryUrl, setDiscoveryUrl] = useState(DEFAULT_DISCOVERY_URL);
  const [checking, setChecking] = useState(false);
  const [authorizing, setAuthorizing] = useState(false);
  const [serverProfiles, setServerProfiles] = useState<ServerProfile[]>([]);
  const [profilesLoading, setProfilesLoading] = useState(false);
  const [serverAuthorization, setServerAuthorization] = useState<CustomServerAuthorization | null>(null);
  const [profileToForget, setProfileToForget] = useState<ServerProfile | null>(null);
  const [profileBusyId, setProfileBusyId] = useState<string | null>(null);
  const [profileError, setProfileError] = useState<string | null>(null);
  const [requestingNotifications, setRequestingNotifications] = useState(false);
  const [discoveryResult, setDiscoveryResult] = useState<"idle" | "untrusted" | "verified" | "failed">("idle");
  const [discoveryMessage, setDiscoveryMessage] = useState("");
  const [discovery, setDiscovery] = useState<DiscoveryDocument | null>(null);
  const [showDeveloper, setShowDeveloper] = useState(false);
  const [gatewayUrl, setGatewayUrl] = useState(import.meta.env.DEV ? "ws://127.0.0.1:18787/v2/realtime" : "wss://realtime.example/v2/realtime");
  const [appId, setAppId] = useState(DEFAULT_APP_ID);
  const [deviceId, setDeviceId] = useState(import.meta.env.DEV ? "device_local" : "device_local_development");
  const isMobilePlatform = runtime.platform === "android" || runtime.platform === "ios";
  const discoveryTransport = setupTransportState(discoveryUrl, "https:");
  const realtimeUrl = discovery?.realtimeUrl ?? discovery?.gatewayUrl ?? "";
  const realtimeTransport = setupTransportState(realtimeUrl, "wss:");
  const turnConfigured = discovery?.iceServers.some((server) => server.urls.some((url) => /^turns?:/i.test(url))) ?? false;

  const loadProfiles = useCallback(async () => {
    if (!runtime.tauri || !runtime.secureStorage) return;
    setProfilesLoading(true);
    try {
      setServerProfiles(await bridge.listServerProfiles());
      setProfileError(null);
    } catch (caught) {
      setProfileError(displayError(caught, "Saved server profiles could not be loaded"));
    } finally {
      setProfilesLoading(false);
    }
  }, [bridge, runtime.secureStorage, runtime.tauri]);

  useEffect(() => { void loadProfiles(); }, [loadProfiles]);

  const testDiscovery = async () => {
    setChecking(true);
    setDiscoveryResult("idle");
    setDiscovery(null);
    onClearError();
    try {
      const result = await bridge.discover(discoveryUrl);
      if (result.signatureVerified && result.schemaVersion === 2) {
        setDiscovery(result);
        setDiscoveryResult("verified");
        setDiscoveryMessage(result.available
          ? `${result.issuer} · ${result.deploymentId} · managed admission ready`
          : `${result.issuer} · ${result.deploymentId} · signature verified, admission not enabled`);
      } else {
        setDiscoveryResult("untrusted");
        setDiscoveryMessage(`Fetched ${result.issuer}, but its signing key is not trusted on this device.`);
      }
    } catch (caught) {
      setDiscoveryResult("failed");
      setDiscoveryMessage(displayError(caught, "Discovery failed"));
    } finally {
      setChecking(false);
    }
  };

  const beginServerAuthorization = async () => {
    if (!discovery?.signatureVerified || !discovery.available || authorizing || connecting) return;
    setAuthorizing(true);
    onClearError();
    setProfileError(null);
    try {
      setServerAuthorization(await bridge.beginCustomServerAuthorization(
        discoveryUrl,
        discovery.appId ? undefined : appId,
      ));
    } catch (caught) {
      setDiscoveryResult("failed");
      setDiscoveryMessage(displayError(caught, "Native server authorization could not begin"));
    } finally {
      setAuthorizing(false);
    }
  };

  const connectProfile = async (profile: ServerProfile) => {
    if (profileBusyId || connecting) return;
    setProfileBusyId(profile.profileId);
    setProfileError(null);
    try { await onConnect(await bridge.connectProfile(profile.profileId)); }
    catch (caught) { setProfileError(displayError(caught, "The saved server profile could not connect")); }
    finally { setProfileBusyId(null); }
  };

  const rotateProfile = async (profile: ServerProfile) => {
    if (profileBusyId) return;
    setProfileBusyId(profile.profileId);
    setProfileError(null);
    try { setServerAuthorization(await bridge.rotateServerTrust(profile.profileId)); }
    catch (caught) { setProfileError(displayError(caught, "Server trust rotation could not begin")); }
    finally { setProfileBusyId(null); }
  };

  const forgetProfile = async (profile: ServerProfile) => {
    if (profileBusyId) return;
    setProfileBusyId(profile.profileId);
    setProfileError(null);
    try {
      await bridge.forgetServerProfile(profile.profileId);
      setProfileToForget(null);
      await loadProfiles();
    } catch (caught) {
      setProfileError(displayError(caught, "The server profile could not be removed"));
    } finally {
      setProfileBusyId(null);
    }
  };

  return (
    <main className="native-setup-root">
      <section className="native-setup-card">
        <div className="setup-brand"><AokieGlyph /><span>Aokie Companion</span></div>
        <span className="section-kicker">SECURE MOBILE RECEPTIONIST</span>
        <h1>Your front desk, wherever you are.</h1>
        <p className="setup-lead">Watch live captions, answer Aokie privately, and—after the secure media bridge is installed—take over a caller without moving the cellular call away from Desktop.</p>

        {isMobilePlatform ? (
          <aside className="same-phone-warning"><AlertTriangle size={20} /><div><strong>This app is a microphone and speaker endpoint</strong><p>Companion never connects to the Bluetooth dongle. Aokie Desktop bridges the caller audio to this app over WebRTC; FormLogic or a compatible custom server authenticates and signals that route. Only when this exact Android device is also carrying the cellular call can its HFP and WebRTC audio routes contend.</p></div></aside>
        ) : (
          <aside className="same-phone-warning is-supported"><ShieldCheck size={20} /><div><strong>This app is a microphone and speaker endpoint</strong><p>Companion uses this computer's selected microphone and speakers. It never connects to the Bluetooth dongle: Aokie Desktop bridges caller audio over WebRTC, while FormLogic or a compatible custom server handles authentication and secure signalling.</p></div></aside>
        )}

        <section className="setup-desktop-pairing" aria-labelledby="setup-desktop-pairing-title">
          <div className="setup-section-heading"><LockKeyhole size={18} /><div><strong id="setup-desktop-pairing-title">Aokie Desktop pairing</strong><small>Owner-confirmed one-use offer and native endpoint proof</small></div></div>
          {managedAdmission && <div className={`setup-result ${managedAdmission.value === "desktop_unavailable" ? "is-warning" : "is-failed"}`} role="status"><AlertTriangle size={17} /><span>{managedAdmission.message}</span></div>}
          <p className="setup-blocked-copy">Pairing binds this active server profile’s existing device ID to the exact Desktop endpoint key. The offer is unsigned public data, so both owners must compare the complete fingerprints shown by the native apps.</p>
          <div className="pairing-import-row">
            <button className="secondary-button" disabled={!runtime.tauri || !runtime.secureStorage} onClick={onOpenDesktopPairing}><LockKeyhole size={17} /> Review Desktop pairing offer</button>
            {managedAdmission && <button className="text-button" disabled={connecting} onClick={() => void onRetryConnection()}>{connecting ? "Waiting for admission…" : "Retry saved connection"}</button>}
          </div>
        </section>

        {(serverProfiles.length > 0 || profilesLoading || profileError) && (
          <section className="saved-server-profiles" aria-labelledby="saved-server-profiles-title">
            <div className="setup-section-heading"><Server size={18} /><div><strong id="saved-server-profiles-title">Saved servers</strong><small>Native profiles keep credentials and endpoint keys outside React</small></div></div>
            {profilesLoading && serverProfiles.length === 0 && <p className="setup-blocked-copy"><RefreshCw className="spin" size={14} /> Loading native profiles…</p>}
            {serverProfiles.map((profile) => (
              <article className="saved-server-profile" key={profile.profileId}>
                <div><strong>{profile.origin}</strong><small>App {profile.appId} · Device {profile.deviceId}</small><code title={profile.discoveryFingerprint}>{profile.discoveryFingerprint}</code></div>
                <span className={profile.active ? "is-active" : ""}>{profile.active ? "Active" : "Trusted"}</span>
                <div className="saved-profile-actions">
                  <button className="secondary-button" disabled={Boolean(profileBusyId) || connecting} onClick={() => void connectProfile(profile)}>{profileBusyId === profile.profileId ? "Working…" : "Connect"}</button>
                  <button className="text-button" disabled={Boolean(profileBusyId)} onClick={() => void rotateProfile(profile)}>Check key</button>
                  <button className="text-button is-danger" disabled={Boolean(profileBusyId)} onClick={() => setProfileToForget(profile)}>Remove</button>
                </div>
              </article>
            ))}
            {profileError && <div className="setup-result is-failed" role="alert"><AlertTriangle size={17} /><span>{profileError}</span></div>}
          </section>
        )}

        <div className="setup-section-heading"><Server size={18} /><div><strong>Connect your deployment</strong><small>Managed FormLogic or a signed self-hosted discovery URL</small></div></div>
        <label className="setup-field">
          <span>Discovery URL</span>
          <input value={discoveryUrl} onChange={(event) => {
            setDiscoveryUrl(event.target.value);
            setDiscovery(null);
            setDiscoveryResult("idle");
          }} inputMode="url" autoCapitalize="none" autoCorrect="off" />
        </label>
        <button className="primary-button" disabled={checking || !discoveryUrl.trim()} onClick={testDiscovery}>
          {checking ? <RefreshCw className="spin" size={17} /> : <ShieldCheck size={17} />}
          {checking ? "Checking signed discovery…" : "Verify server"}
        </button>
        {discoveryResult !== "idle" && (
          <div className={`setup-result is-${discoveryResult}`} role="status">
            {discoveryResult === "verified" ? <CheckCircle2 size={18} /> : <AlertTriangle size={18} />}
            <span>{discoveryMessage}</span>
          </div>
        )}
        {discoveryResult === "untrusted" && <p className="setup-blocked-copy">Enrollment stays locked until the deployment root key is installed and the native signature verifier accepts this document.</p>}
        {discovery?.signatureVerified && !discovery.available && <p className="setup-blocked-copy">This server is authentic, but its administrator has not enabled Companion OAuth/admission yet. A signed gateway URL and admission endpoint must be published before managed sign-in can start.</p>}
        {discovery?.signatureVerified && discovery.available && (
          <div className="developer-fields">
            {discovery.appId ? <p>App <strong>{discovery.appId}</strong> is bound by the signed discovery document.</p> : <label className="setup-field"><span>App ID</span><input value={appId} onChange={(event) => setAppId(event.target.value)} /></label>}
            <button className="primary-button" disabled={authorizing || connecting || !runtime.tauri || !runtime.secureStorage || (!discovery.appId && !appId.trim())} onClick={() => void beginServerAuthorization()}>
              {authorizing || connecting ? <RefreshCw className="spin" size={17} /> : <LockKeyhole size={17} />}
              {authorizing ? "Preparing native trust check…" : connecting ? "Connecting…" : "Review trust, sign in and connect"}
            </button>
            {!runtime.tauri && <p className="setup-blocked-copy">Managed sign-in runs only inside the native Companion app; browser preview cannot receive or store its credentials.</p>}
            {runtime.tauri && !runtime.secureStorage && <p className="setup-blocked-copy">This platform build does not have a native credential store. FormLogic and compatible custom-server sign-in remain locked rather than placing credentials in web storage.</p>}
          </div>
        )}

        <div className="runtime-grid" aria-label="Runtime readiness">
          <RuntimeChip icon={Wifi} label="Realtime" ready={runtime.realtime} />
          <RuntimeChip icon={LockKeyhole} label="Secure store" ready={runtime.secureStorage} />
          <RuntimeChip icon={Smartphone} label="Native calls" ready={runtime.nativeCallUi} />
          <RuntimeChip icon={Radio} label="Media bridge" ready={runtime.mediaBridge} />
        </div>

        {discovery?.signatureVerified && (
          <section className="setup-readiness" aria-labelledby="setup-readiness-title">
            <div className="setup-section-heading"><ShieldCheck size={18} /><div><strong id="setup-readiness-title">Connection readiness</strong><small>Verified facts only; Desktop presence is checked after sign-in</small></div></div>
            <SetupReadinessRow ready label="Signed discovery" detail={`Schema v${discovery.schemaVersion} · ${discovery.deploymentId}`} />
            <SetupReadinessRow ready={discoveryTransport.ready} warning={discoveryTransport.developmentOnly} label="Server transport" detail={discoveryTransport.detail} />
            <SetupReadinessRow ready={realtimeTransport.ready} warning={realtimeTransport.developmentOnly} label="Realtime signalling" detail={realtimeTransport.detail} />
            <SetupReadinessRow ready={turnConfigured} warning={!turnConfigured} label="TURN fallback" detail={turnConfigured ? "Encrypted relay is advertised for networks where direct WebRTC cannot connect" : "No TURN relay is advertised; direct WebRTC may still work"} />
            <SetupReadinessRow ready={!isMobilePlatform || runtime.pushRegistration === "registered"} warning={isMobilePlatform && runtime.pushRegistration !== "registered"} label="Background offers" detail={isMobilePlatform ? `Native push: ${runtime.pushRegistration.replace(/_/g, " ")}` : "Foreground Windows endpoint; no mobile push provider required"} />
            <SetupReadinessRow label="Aokie Desktop" detail="Checked through authenticated realtime after this endpoint is approved" />
          </section>
        )}

        {runtime.platform === "android" && runtime.notificationPermission !== "granted" && runtime.notificationPermission !== "not_required" && (
          <div className="developer-fields">
            <p>Android notifications are currently denied. Genuine voice offers cannot start Core-Telecom or a foreground call surface until you allow them. Microphone access remains a separate, later prompt used only after an active talk lease.</p>
            <button className="secondary-button" disabled={requestingNotifications} onClick={() => {
              setRequestingNotifications(true);
              void bridge.requestNotificationPermission()
                .then(() => onRefreshRuntime())
                .catch((caught) => setDiscoveryMessage(displayError(caught, "Notification permission could not be requested")))
                .finally(() => setRequestingNotifications(false));
            }}>
              {requestingNotifications ? "Waiting for Android…" : "Allow call notifications"}
            </button>
          </div>
        )}
        {runtime.platform === "android" && runtime.pushRegistration === "configuration_required" && (
          <p className="setup-blocked-copy">This build has no Firebase project configuration. Foreground realtime still works, but background or terminated ringing requires a private <code>google-services.json</code> build and native endpoint registration.</p>
        )}
        {runtime.platform === "android" && runtime.batteryOptimizationsRestricted && (
          <p className="setup-blocked-copy">Android battery optimization is active. High-priority push wake is best-effort, and force-stop cannot be detected or bypassed.</p>
        )}

        {error && <div className="setup-result is-failed"><AlertTriangle size={18} /><span>{error}</span></div>}

        {import.meta.env.DEV && <section className="developer-connect">
            <button className="text-button" onClick={() => setShowDeveloper((value) => !value)}>{showDeveloper ? "Hide" : "Show"} {import.meta.env.DEV ? "local gateway test" : "custom server connection"}</button>
            {showDeveloper && <div className="developer-fields">
              <p>{runtime.tauri ? "Development only. Launch the native process with AOKIE_COMPANION_LOCAL_TOKEN; the renderer never accepts or receives that test authority." : "Open this build through the native Companion runtime to connect a gateway. Browser preview cannot connect."}</p>
              <label className="setup-field"><span>Gateway WebSocket</span><input value={gatewayUrl} onChange={(event) => setGatewayUrl(event.target.value)} /></label>
              <label className="setup-field"><span>App ID</span><input value={appId} onChange={(event) => setAppId(event.target.value)} /></label>
              <label className="setup-field"><span>Device ID</span><input value={deviceId} onChange={(event) => setDeviceId(event.target.value)} /></label>
              <button className="secondary-button" disabled={connecting || !runtime.tauri || !isDebugLoopback(gatewayUrl)} onClick={() => void onConnect({ gatewayUrl, appId, deviceId, accessToken: "", protocolVersion: 2 })}>{connecting ? "Connecting…" : "Connect native-injected test gateway"}</button>
            </div>}
          </section>}

        <div className="or-divider"><span>or</span></div>
        <button className="secondary-button" onClick={onDemo}>Explore the interactive demo</button>
        <p className="setup-footnote">The demo never requests notification or microphone permission and cannot contact a caller.</p>
      </section>
      {serverAuthorization && (
        <ServerTrustDialog
          authorization={serverAuthorization}
          busy={authorizing || connecting}
          error={profileError}
          onDecision={async (approved) => {
            if (!approved) {
              await bridge.confirmCustomServerTrust(serverAuthorization, false).catch(() => undefined);
              setServerAuthorization(null);
              setProfileError(null);
              return;
            }
            setAuthorizing(true);
            setProfileError(null);
            try {
              const confirmed = await bridge.confirmCustomServerTrust(serverAuthorization, true);
              setServerAuthorization(null);
              await loadProfiles();
              await onConnect(await bridge.connectProfile(confirmed.profileId));
            } catch (caught) {
              setProfileError(displayError(caught, "Native server trust or sign-in could not be completed"));
            } finally {
              setAuthorizing(false);
            }
          }}
          onExpired={() => { setServerAuthorization(null); setProfileError(null); }}
        />
      )}
      {profileToForget && (
        <ForgetServerProfileDialog
          profile={profileToForget}
          busy={profileBusyId === profileToForget.profileId}
          onCancel={() => { if (!profileBusyId) setProfileToForget(null); }}
          onConfirm={() => forgetProfile(profileToForget)}
        />
      )}
    </main>
  );
}

function ServerTrustDialog({ authorization, busy, error, onDecision, onExpired }: {
  authorization: CustomServerAuthorization;
  busy: boolean;
  error: string | null;
  onDecision(approved: boolean): Promise<void>;
  onExpired(): void;
}) {
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1_000));
  const remaining = Math.max(0, authorization.expiresAt - now);
  const changed = authorization.trustState === "rotation_required" || authorization.trustState === "identity_changed";
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Math.floor(Date.now() / 1_000)), 1_000);
    return () => window.clearInterval(timer);
  }, [authorization.authorizationId]);
  return (
    <div className="modal-layer is-centered server-trust-modal" role="presentation">
      <section className="modal-sheet is-dialog" role="dialog" aria-modal="true" aria-labelledby="server-trust-title">
        <div className={changed ? "danger-modal-icon" : "takeover-hero-icon"}>{changed ? <AlertTriangle size={24} /> : <ShieldCheck size={24} />}</div>
        <span className="section-kicker">SIGNED SERVER TRUST</span>
        <h2 id="server-trust-title">{authorization.trustState === "first_use" ? "Trust this Companion server?" : changed ? "Server identity needs approval" : "Re-authorize this trusted server?"}</h2>
        <p className="modal-lead">Native code fetched and verified signed schema-v2 discovery from <strong>{authorization.origin}</strong>. Compare its full signing-key fingerprint through a separate trusted channel before continuing.</p>
        {authorization.previousDiscoveryFingerprint && <div className="peer-fingerprint is-previous"><span>Previously trusted server key</span><code>{authorization.previousDiscoveryFingerprint}</code></div>}
        <div className="peer-fingerprint"><span>Current server signing key</span><code>{authorization.discoveryFingerprint}</code></div>
        <div className="peer-fingerprint is-endpoint"><span>This Companion installation</span><code>{authorization.endpointFingerprint}</code></div>
        <div className="end-caller-safety-list"><span><LockKeyhole size={16} /> OAuth and credentials stay native</span><span><ShieldCheck size={16} /> Server handles signalling, never call PCM</span><span><RefreshCw size={16} /> Confirmation expires in {remaining}s</span></div>
        {authorization.trustState === "identity_changed" && <div className="setup-result is-failed" role="alert"><AlertTriangle size={17} /><span>This installation identity changed. Continue only if you deliberately reset or reinstalled Companion.</span></div>}
        {remaining <= 0 && <div className="setup-result is-failed" role="alert"><AlertTriangle size={17} /><span>This trust comparison expired. Start again to fetch a fresh signed document.</span></div>}
        {error && <div className="setup-result is-failed" role="alert"><AlertTriangle size={17} /><span>{error}</span></div>}
        <button className="primary-button" disabled={busy || remaining <= 0} onClick={() => void onDecision(true)}><ShieldCheck size={18} />{busy ? "Waiting for native sign-in…" : changed ? "Approve exact key and sign in" : "Approve and sign in"}</button>
        <button className="text-button" disabled={busy} onClick={() => remaining <= 0 ? onExpired() : void onDecision(false)}>{remaining <= 0 ? "Close expired comparison" : "Reject server"}</button>
      </section>
    </div>
  );
}

function ForgetServerProfileDialog({ profile, busy, onCancel, onConfirm }: {
  profile: ServerProfile;
  busy: boolean;
  onCancel(): void;
  onConfirm(): Promise<void>;
}) {
  return (
    <div className="modal-layer is-centered server-trust-modal" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget && !busy) onCancel(); }}>
      <section className="modal-sheet is-dialog" role="dialog" aria-modal="true" aria-labelledby="forget-server-profile-title">
        <div className="danger-modal-icon"><Server size={24} /></div>
        <h2 id="forget-server-profile-title">Remove this trusted server?</h2>
        <p className="modal-lead">Companion will first attempt to unregister the active native session and push endpoint, then remove the local OAuth credentials, server pin, and profile for <strong>{profile.origin}</strong>.</p>
        <button className="danger-button" disabled={busy} onClick={() => void onConfirm()}>{busy ? "Removing native profile…" : "Sign out and remove server"}</button>
        <button className="text-button" disabled={busy} onClick={onCancel}>Keep this server</button>
      </section>
    </div>
  );
}

function isDebugLoopback(value: string): boolean {
  if (!import.meta.env.DEV) return false;
  try {
    const url = new URL(value);
    return url.protocol === "ws:" && ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
  } catch {
    return false;
  }
}

function setupTransportState(value: string, secureProtocol: "https:" | "wss:"): { ready: boolean; developmentOnly: boolean; detail: string } {
  try {
    const url = new URL(value);
    if (url.protocol === secureProtocol) return { ready: true, developmentOnly: false, detail: `${secureProtocol.slice(0, -1).toUpperCase()} transport verified by the native client` };
    const loopback = ["localhost", "127.0.0.1", "[::1]", "formlogic.local", "api.formlogic.local"].includes(url.hostname);
    if (import.meta.env.DEV && loopback) return { ready: true, developmentOnly: true, detail: `Development-only ${url.protocol.replace(":", "").toUpperCase()} exception for ${url.hostname}` };
    return { ready: false, developmentOnly: false, detail: `Release enrollment requires ${secureProtocol}//` };
  } catch {
    return { ready: false, developmentOnly: false, detail: "No valid endpoint was advertised" };
  }
}

function SetupReadinessRow({ label, detail, ready = false, warning = false }: { label: string; detail: string; ready?: boolean; warning?: boolean }) {
  return <div className={`setup-readiness-row ${ready ? "is-ready" : warning ? "is-warning" : "is-pending"}`}>{ready ? <CheckCircle2 size={17} /> : warning ? <AlertTriangle size={17} /> : <RefreshCw size={17} />}<span><strong>{label}</strong><small>{detail}</small></span></div>;
}

function RuntimeChip({ icon: Icon, label, ready }: { icon: typeof Wifi; label: string; ready: boolean }) {
  return <span className={ready ? "is-ready" : "is-pending"}><Icon size={15} />{label}<small>{ready ? "Ready" : "Not installed"}</small></span>;
}

interface V2LiveRuntimeProps {
  runtime: RuntimeCapabilities;
  deviceId: string;
  transport: "idle" | "connecting" | "connected" | "reconnecting" | "offline";
  snapshot: V2CallSnapshotEvent | null;
  lease: V2LeaseEvent | null;
  assistance: V2AssistanceRequestEvent | null;
  endCaller: V2EndCallerEvent | null;
  nativeMediaState: NativeMediaStateEvent | null;
  nativeMediaLevels: NativeMediaLevelsEvent | null;
  localMediaProof: LocalMediaProof | null;
  autoArmInFlight: boolean;
  audioEndpointControls: ReactNode;
  onOpenCalls(): void;
  callRecordId: string | null;
  onRequestLease(mode: V2LeaseMode, acceptedTransferRequestId?: string): Promise<{ requestId: string }>;
  onRevokeLease(reason: string): Promise<{ requestId: string }>;
  onAnswerAssistance(requestId: string, answer: string, responseAction?: "answer" | "decline"): Promise<{ requestId: string; answerId: string }>;
  onPrepareEndCaller(): Promise<{ requestId: string }>;
  onConfirmEndCaller(confirmationId: string): Promise<{ requestId: string }>;
  onSetMicrophoneMuted(muted: boolean): Promise<{ requestId: string }>;
  onReportFailure(message: string, operation: V2RuntimeFailureOperation): void;
  onBeginTakeover(): number;
  onConfirmTakeoverTarget(generation: number, target: ConfirmedTakeoverTarget): void;
  onTakeoverEnqueued(generation: number): void;
  onBack(): Promise<void>;
}

export interface V2ProgressStage {
  title: string;
  detail: string;
  status: "done" | "active" | "waiting";
}

function markSequentialProgress(stages: Array<Omit<V2ProgressStage, "status"> & { proven: boolean }>): V2ProgressStage[] {
  const firstUnproven = stages.findIndex((stage) => !stage.proven);
  return stages.map((stage, index) => ({
    title: stage.title,
    detail: stage.detail,
    status: firstUnproven === -1 || index < firstUnproven ? "done" : index === firstUnproven ? "active" : "waiting",
  }));
}

export function v2RouteProgressStages(
  mode: "consult" | "takeover",
  serviceMode: V2CallSnapshotEvent["snapshot"]["serviceMode"] | undefined,
  leasePhase: V2LeaseEvent["phase"] | null,
  remoteAudioReady: boolean,
  finalRouteProven: boolean,
): V2ProgressStage[] {
  const holdProven = mode === "consult"
    ? serviceMode === "soft_hold" || serviceMode === "consult_pending" || serviceMode === "consult_active"
    : serviceMode === "soft_hold" || serviceMode === "human_pending" || serviceMode === "human_active";
  return markSequentialProgress([
    { title: "Signed request admitted", detail: "The gateway published this device's one-use media lease", proven: leasePhase !== null },
    { title: "Caller safely isolated", detail: mode === "consult" ? "Desktop published a private Aokie consult state" : "Desktop published hold or human-pending ownership", proven: holdProven },
    { title: "Secure receive path ready", detail: "The native endpoint proved current remote audio", proven: remoteAudioReady },
    { title: mode === "consult" ? "Private microphone route proven" : "Caller talk route proven", detail: "Only the exact current lease may arm this microphone", proven: finalRouteProven },
  ]);
}

export function v2RecoveryProgressStages(
  transport: V2LiveRuntimeProps["transport"],
  hasCurrentLease: boolean,
  microphoneActive: boolean,
): V2ProgressStage[] {
  return markSequentialProgress([
    { title: "Caller safely held", detail: "Aokie Desktop published the authoritative recovery state", proven: true },
    { title: "Old media lease retired", detail: "The prior route cannot be reused after connection loss", proven: !hasCurrentLease },
    { title: "Microphone proof removed", detail: "Companion treats an unproven microphone as blocked", proven: !microphoneActive },
    {
      title: transport === "connected" ? "Reconcile a fresh route" : "Reconnect signed signalling",
      detail: transport === "connected" ? "Waiting for a newer authoritative Desktop state" : "All live controls remain locked while reconnecting",
      proven: false,
    },
  ]);
}

export function assistanceMatchesV2Call(
  assistance: V2AssistanceRequestEvent | null,
  call: V2CallSnapshotEvent["snapshot"] | null | undefined,
): boolean {
  return Boolean(
    assistance && call && call.telephonyState === "active" && call.serviceMode !== "ended" &&
    assistance.callId === call.callId && assistance.callEpoch === call.callEpoch &&
    assistance.ownerEpoch === call.ownerEpoch && assistance.switchboardRevision === call.switchboardRevision &&
    assistance.remoteRevision === call.remoteRevision,
  );
}

export function v2TransferTranscript(
  snapshot: V2CallSnapshotEvent | null,
  assistance: V2AssistanceRequestEvent | null,
  maximumTurns = 8,
  nowSeconds = Math.floor(Date.now() / 1_000),
): { available: boolean; reason?: string; captions: NonNullable<V2CallSnapshotEvent["snapshot"]["captions"]> } {
  const call = snapshot?.snapshot;
  if (!assistance?.transferOffered || !Number.isSafeInteger(nowSeconds) || assistance.expiresAt <= nowSeconds ||
    snapshot?.appId !== assistance.appId || !assistanceMatchesV2Call(assistance, call)
  ) {
    return { available: false, reason: "The transfer no longer matches current authenticated call state.", captions: [] };
  }
  if (!snapshot.grants.includes("assistance_read") || !snapshot.grants.includes("captions_read")) {
    return { available: false, reason: "Transcript access was not granted for this device.", captions: [] };
  }
  if (!call?.remoteConsent.enabled || !call.remoteConsent.acknowledged ||
    !call.remoteConsent.assistanceEnabled || !call.remoteConsent.captionsEnabled
  ) {
    return { available: false, reason: "The current caller disclosure policy does not permit transcript access.", captions: [] };
  }
  const limit = Number.isSafeInteger(maximumTurns) ? Math.min(12, Math.max(1, maximumTurns)) : 8;
  const captions = (call.captions ?? [])
    .filter((caption) => caption.finalText && caption.text.trim().length > 0)
    .slice(-limit);
  return {
    available: true,
    reason: captions.length ? undefined : "No permission-filtered transcript turns have arrived yet.",
    captions,
  };
}

export function v2AssistanceGrantAccess(grants: Iterable<V2Grant>): {
  readable: boolean;
  answerable: boolean;
} {
  const current = new Set(grants);
  const readable = current.has("assistance_read");
  return {
    readable,
    answerable: readable && current.has("assistance_respond"),
  };
}

export function v2RemoteAudioProofLabel(
  mode: NativeMediaSession["mode"],
  ready: boolean,
): string {
  if (mode === "consult" || mode === "prepared_consult") {
    return ready ? "Private Aokie audio ready" : "Waiting for private Aokie audio";
  }
  return ready ? "Caller audio ready" : "Waiting for caller audio";
}

function V2LiveRuntime({ runtime, deviceId, transport, snapshot, lease, assistance, endCaller, nativeMediaState, nativeMediaLevels, localMediaProof, autoArmInFlight, audioEndpointControls, onOpenCalls, callRecordId, onRequestLease, onRevokeLease, onAnswerAssistance, onPrepareEndCaller, onConfirmEndCaller, onSetMicrophoneMuted, onReportFailure, onBeginTakeover, onConfirmTakeoverTarget, onTakeoverEnqueued, onBack }: V2LiveRuntimeProps) {
  const [busy, setBusy] = useState(false);
  const [assistanceAnswer, setAssistanceAnswer] = useState("");
  const [assistanceAwaitingAcknowledgement, setAssistanceAwaitingAcknowledgement] = useState<string | null>(null);
  const [microphoneMutation, setMicrophoneMutation] = useState<"muting" | "unmuting" | null>(null);
  const [takeoverConfirmationOpen, setTakeoverConfirmationOpen] = useState(false);
  const [acceptedTransferRequestId, setAcceptedTransferRequestId] = useState<string | null>(null);
  const latestSnapshot = useRef(snapshot);
  const latestLease = useRef(lease);
  latestSnapshot.current = snapshot;
  latestLease.current = lease;
  const [endConfirmationOpen, setEndConfirmationOpen] = useState(false);
  const [endConfirmationPending, setEndConfirmationPending] = useState(false);
  const [endConfirmationId, setEndConfirmationId] = useState<string | null>(null);
  const [nowSeconds, setNowSeconds] = useState(() => Math.floor(Date.now() / 1_000));
  const connected = transport === "connected" && Boolean(snapshot);
  const grants = new Set(snapshot?.grants ?? []);
  const call = snapshot?.snapshot;
  const monitorOffer = currentV2InAppOffer(snapshot, deviceId, "monitor", nowSeconds);
  const consultOffer = currentV2InAppOffer(snapshot, deviceId, "consult", nowSeconds);
  const takeoverOffer = currentV2InAppOffer(snapshot, deviceId, "takeover", nowSeconds);
  const transferOffer = assistance?.transferOffered
    ? currentV2InAppOffer(snapshot, deviceId, "takeover", nowSeconds, assistance.requestId)
    : null;
  const mediaUsable = runtime.mediaBridge && connected && call?.telephonyState === "active" && call.mediaState !== "none" && call.mediaState !== "failed" && grants.has("rtc_signal");
  const currentConsent = v2RemoteConsentCurrent(
    connected,
    call?.remoteConsent,
    nowSeconds * 1_000,
  );
  const currentAccessPolicy = v2CurrentAccessPolicyPresentation(snapshot, currentConsent);
  const monitorAllowed = Boolean(monitorOffer && mediaUsable && currentConsent && call?.remoteConsent.monitorEnabled && grants.has("monitor"));
  const takeoverAllowed = Boolean(takeoverOffer && mediaUsable && currentConsent && call?.remoteCapabilities.takeover && call.remoteConsent.takeoverEnabled && grants.has("takeover") && grants.has("resume_aokie"));
  const assistanceMatchesCall = assistanceMatchesV2Call(assistance, call);
  const assistanceAccess = v2AssistanceGrantAccess(grants);
  const assistanceReadable = Boolean(connected && assistanceMatchesCall && currentConsent && call?.remoteConsent.assistanceEnabled && assistanceAccess.readable);
  const assistanceAllowed = assistanceReadable && assistanceAccess.answerable;
  const transferAllowed = Boolean(
    assistance?.transferOffered && transferOffer && assistanceAllowed && mediaUsable &&
    call?.remoteCapabilities.takeover && call.remoteConsent.takeoverEnabled &&
    grants.has("takeover") && grants.has("resume_aokie") && call.serviceMode === "aokie_active",
  );
  const assistanceRemaining = assistanceMatchesCall && assistance ? Math.max(0, assistance.expiresAt - nowSeconds) : 0;
  const consultAllowed = Boolean(
    consultOffer && mediaUsable && currentConsent && call?.remoteCapabilities.softwareHold && call.remoteCapabilities.voiceConsult &&
    call.remoteConsent.consultEnabled && grants.has("consult") &&
    assistanceReadable && assistance && assistanceRemaining > 0 && call?.serviceMode === "aokie_active",
  );

  useEffect(() => {
    setAssistanceAnswer("");
    setAssistanceAwaitingAcknowledgement(null);
    setAcceptedTransferRequestId(null);
    setTakeoverConfirmationOpen(false);
  }, [assistance?.requestId]);
  useEffect(() => {
    if (!assistance && endCaller?.kind !== "end_caller_challenge" && !call?.pendingMobileOffers.length) return;
    const timer = window.setInterval(() => setNowSeconds(Math.floor(Date.now() / 1_000)), 1_000);
    return () => window.clearInterval(timer);
  }, [assistance, call?.pendingMobileOffers.length, endCaller]);
  const exactNativeSession = isExactCurrentV2NativeMediaSession(lease, nativeMediaState);
  const exactNativeLevels = currentConsent && isExactCurrentV2NativeMediaSession(lease, nativeMediaLevels)
    ? nativeMediaLevels
    : null;
  const authoritativeMicrophoneMuted = call?.companionMicrophoneMuted === true;
  const canArmMicrophone = Boolean(
    exactNativeSession && lease?.phase === "active" &&
    (lease.mode === "takeover" || lease.mode === "consult") &&
    nativeMediaState?.remoteAudioReady &&
    ARMABLE_MEDIA_PHASES.includes(nativeMediaState.phase),
  );
  const canToggleMicrophone = Boolean(
    exactNativeSession && lease?.phase === "active" &&
    (lease.mode === "takeover" || lease.mode === "consult") &&
    ((authoritativeMicrophoneMuted && canArmMicrophone) ||
      (!authoritativeMicrophoneMuted && nativeMediaState?.microphoneActive)),
  );
  const truthfulMonitorProof = Boolean(
    lease?.mode === "monitor" && lease.phase === "active" && exactNativeSession &&
    nativeMediaState?.remoteAudioReady &&
    ["connected", "remote_audio_ready", "answer_applied"].includes(nativeMediaState.phase),
  );
  const exactLocalMediaProof = Boolean(
    lease && localMediaProof?.active &&
    localMediaProof.appId === lease.session.appId &&
    localMediaProof.streamNonce === lease.session.streamNonce &&
    localMediaProof.rtcSessionId === lease.session.rtcSessionId &&
    localMediaProof.callId === lease.session.callId &&
    localMediaProof.callEpoch === lease.session.callEpoch &&
    localMediaProof.ownerEpoch === lease.session.ownerEpoch &&
    localMediaProof.deviceId === lease.session.deviceId &&
    localMediaProof.leaseId === lease.session.leaseId &&
    localMediaProof.fence === lease.session.fence &&
    localMediaProof.sdpRevision === lease.session.sdpRevision &&
    localMediaProof.transportGeneration === lease.session.transportGeneration &&
    isCurrent(localMediaProof.expiresAt)
  );
  const truthfulTalkProof = Boolean(
    lease?.mode === "takeover" && lease.phase === "active" && exactNativeSession &&
    call?.mediaState === "active" && !authoritativeMicrophoneMuted &&
    nativeMediaState?.microphoneActive && nativeMediaState.remoteAudioReady && exactLocalMediaProof &&
    localMediaProof?.mode === "talk",
  );
  const truthfulConsultProof = Boolean(
    lease?.mode === "consult" && lease.phase === "active" && exactNativeSession &&
    call?.serviceMode === "consult_active" && !authoritativeMicrophoneMuted && nativeMediaState?.microphoneActive &&
    nativeMediaState.remoteAudioReady && exactLocalMediaProof && localMediaProof?.mode === "consult",
  );
  const truthfulTalkMuted = Boolean(
    lease?.mode === "takeover" && lease.phase === "active" && exactNativeSession &&
    call?.serviceMode === "human_active" && call.mediaState === "active" &&
    authoritativeMicrophoneMuted &&
    nativeMediaState?.phase === "microphone_disarmed" &&
    !nativeMediaState.microphoneActive && nativeMediaState.remoteAudioReady,
  );
  const activeTakeoverOwned = Boolean(
    transport === "connected" && grants.has("end_caller") && grants.has("takeover") &&
    currentConsent && call?.remoteConsent.takeoverEnabled && lease?.mode === "takeover" &&
    lease.phase === "active" && lease.session.fence > 0 && exactNativeSession &&
    snapshot?.appId === lease.session.appId && call?.callId === lease.session.callId &&
    call.callEpoch === lease.session.callEpoch && call.ownerEpoch === lease.session.ownerEpoch &&
    call.serviceMode === "human_active" && call.telephonyState === "active",
  );
  const leaseRevokeAllowed = v2LeaseRevokeAllowed(connected, lease);
  const currentEndChallenge = isCurrentEndCallerChallenge(endCaller, snapshot, lease, nowSeconds)
    ? endCaller
    : null;
  const endCallerSubmitted = endCaller?.kind === "end_caller_submitted" && endCaller.confirmationId === endConfirmationId;
  const endCallerCompleted = endCaller?.kind === "end_caller_result" && endCaller.outcome === "completed" && endCaller.confirmationId === endConfirmationId;
  const safety = v2SafetyPresentation(
    transport,
    call?.serviceMode,
    lease,
    truthfulMonitorProof,
    truthfulTalkProof,
    truthfulConsultProof,
    truthfulTalkMuted,
  );
  const pendingRouteMode = lease && (lease.mode === "consult" || lease.mode === "takeover") && (
    lease.phase === "prepared" || call?.serviceMode === "soft_hold" ||
    call?.serviceMode === "consult_pending" || call?.serviceMode === "human_pending"
  ) ? lease.mode : null;

  const run = async (
    action: () => Promise<unknown>,
    operation: V2RuntimeFailureOperation = "request",
    fallback = "The media request failed",
  ): Promise<boolean> => {
    if (busy) return false;
    setBusy(true);
    try {
      await action();
      return true;
    } catch (caught) {
      onReportFailure(displayError(caught, fallback), operation);
      return false;
    } finally {
      setBusy(false);
    }
  };

  const confirmEndCaller = async (challenge: V2EndCallerChallengeEvent) => {
    if (busy || endConfirmationPending || !isCurrentEndCallerChallenge(challenge, snapshot, lease, Math.floor(Date.now() / 1_000))) return;
    setBusy(true);
    setEndConfirmationPending(true);
    setEndConfirmationOpen(false);
    try {
      await onConfirmEndCaller(challenge.confirmationId);
    } catch (caught) {
      setEndConfirmationPending(false);
      setEndConfirmationOpen(true);
      onReportFailure(displayError(caught, "The caller call could not be ended. The call remains connected."), "end_caller");
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    if (!takeoverAllowed || lease || call?.serviceMode !== "aokie_active" || transport !== "connected") {
      setTakeoverConfirmationOpen(false);
    }
  }, [call?.serviceMode, lease, takeoverAllowed, transport]);

  useEffect(() => {
    if (currentEndChallenge) {
      setEndConfirmationId(currentEndChallenge.confirmationId);
      setEndConfirmationOpen(true);
    } else setEndConfirmationOpen(false);
  }, [currentEndChallenge]);

  useEffect(() => {
    if (!activeTakeoverOwned && !endCallerCompleted) setEndConfirmationId(null);
  }, [activeTakeoverOwned, endCallerCompleted]);

  useEffect(() => {
    if (endCaller?.kind === "end_caller_submitted" || endCaller?.kind === "end_caller_result" || endCaller?.kind === "end_caller_failure") {
      setEndConfirmationPending(false);
    }
    if (endCaller?.kind === "end_caller_failure") onReportFailure(endCaller.message, "end_caller");
    if (endCaller?.kind === "end_caller_result" && endCaller.outcome === "failed") {
      onReportFailure(endCaller.message ?? "Aokie Desktop could not end the caller call. The call remains connected.", "end_caller");
    }
  }, [endCaller, onReportFailure]);

  return (
    <main className="live-runtime-root">
      <header className="runtime-header"><button className="icon-button" aria-label="Back to Companion home" onClick={() => void onBack()}><ArrowLeft size={20} /></button><div><span className="section-kicker">AOKIE COMPANION · V2</span><strong>Independent audio endpoint</strong></div><span className={`transport-pill is-${transport}`}>{transport === "connected" ? <Wifi size={14} /> : <WifiOff size={14} />}{transport}</span></header>
      {!snapshot ? (
        <section className="runtime-empty"><RefreshCw className={transport !== "offline" && transport !== "connected" ? "spin" : ""} size={32} /><h1>{transport === "connected" ? "Connected — waiting for a live Aokie call" : "Waiting for signed admission and call state"}</h1><p>{transport === "connected" ? "The authenticated idle state is current. No microphone, speaker route, or call controls are active." : "Media controls stay locked until the v2 gateway publishes authenticated call state."}</p></section>
      ) : (
        <>
          <section className={`runtime-safety is-${call?.serviceMode}`} aria-live="assertive" data-testid="authoritative-audio-banner">
            {truthfulTalkProof || truthfulConsultProof ? <Mic size={22} /> : lease?.phase === "prepared" || transport !== "connected" ? <RefreshCw className={transport === "connected" ? "spin" : ""} size={22} /> : <ShieldCheck size={22} />}
            <div><strong>{safety.title}</strong><small>{safety.detail}</small></div>
          </section>
          <section className="runtime-caller"><span>{initials(call?.caller?.label)}</span><div><h1>{call?.caller?.label ?? "Caller identity hidden"}</h1><p>{call?.caller?.maskedNumber ?? "Caller ID permission not granted"}</p></div><em>{call?.telephonyState}</em></section>
          {call?.serviceMode === "recovering" && <V2RecoveryPanel transport={transport} lease={lease} nativeMediaState={nativeMediaState} />}
          {call?.serviceMode !== "recovering" && call?.serviceMode !== "ended" && transport !== "connected" && <V2UnconfirmedCallPanel transport={transport} />}
          {call?.serviceMode === "ended" && <V2CallEndedPanel onOpenCalls={onOpenCalls} callRecordId={callRecordId} />}
          {pendingRouteMode && <V2RouteProgressPanel
            mode={pendingRouteMode}
            serviceMode={call?.serviceMode}
            leasePhase={lease?.phase ?? null}
            remoteAudioReady={Boolean(exactNativeSession && nativeMediaState?.remoteAudioReady)}
            finalRouteProven={pendingRouteMode === "consult" ? truthfulConsultProof : truthfulTalkProof}
          />}
          {truthfulConsultProof && <section className="runtime-proof" aria-label="Private consultation isolation"><span><LockKeyhole size={14} /> Caller held by Desktop</span><span><Mic size={14} /> Microphone routed only to Aokie</span></section>}
          <section className="runtime-proof"><span><ShieldCheck size={14} /> Call epoch {call?.callEpoch}</span><span><LockKeyhole size={14} /> Owner epoch {call?.ownerEpoch}</span><span><RefreshCw size={14} /> Sequence {snapshot.sequence}</span></section>
          <section className="runtime-proof" aria-label="Remote consent policy"><span><ShieldCheck size={14} /> Policy {call?.remoteConsent.policyId} v{call?.remoteConsent.policyVersion}</span><span><LockKeyhole size={14} /> {currentConsent ? "Disclosure acknowledged" : "Remote access not acknowledged"}</span></section>
          {currentAccessPolicy && <aside className="media-gate" aria-label="Current access policy"><LockKeyhole size={19} /><div><strong>{currentAccessPolicy.title}</strong><p>{currentAccessPolicy.detail}</p></div></aside>}
          {nativeMediaState && <section className="runtime-proof" aria-label="Native media state"><span><Radio size={14} /> {nativeMediaState.session.mode} · {nativeMediaState.phase}</span><span><Headphones size={14} /> {v2RemoteAudioProofLabel(nativeMediaState.session.mode, nativeMediaState.remoteAudioReady)}</span><span><Mic size={14} /> {authoritativeMicrophoneMuted && !nativeMediaState.microphoneActive ? "Microphone muted by Desktop" : nativeMediaState.microphoneActive ? "Microphone armed" : "Microphone blocked"}</span></section>}
          {call?.telephonyState !== "ended" && audioEndpointControls}
          {assistance && <V2AssistanceRequestCard
            assistance={assistance}
            transferTranscript={v2TransferTranscript(snapshot, assistance)}
            remainingSeconds={assistanceRemaining}
            answer={assistanceAnswer}
            available={assistanceAllowed}
            busy={busy}
            awaitingAcknowledgement={assistanceAwaitingAcknowledgement === assistance.requestId}
            onAnswerChange={setAssistanceAnswer}
            onSubmit={() => {
              const requestId = assistance.requestId;
              const answer = assistanceAnswer.trim();
              void run(
                () => onAnswerAssistance(requestId, answer),
                "request",
                "The private answer could not be queued",
              ).then((accepted) => {
                if (accepted) setAssistanceAwaitingAcknowledgement(requestId);
              });
            }}
            transferAvailable={transferAllowed}
            onAcceptTransfer={() => {
              setAcceptedTransferRequestId(assistance.requestId);
              setTakeoverConfirmationOpen(true);
            }}
            onDeclineTransfer={() => {
              const requestId = assistance.requestId;
              const answer = assistanceAnswer.trim() || "declined";
              void run(
                () => onAnswerAssistance(requestId, answer, "decline"),
                "request",
                "The transfer decline could not be queued",
              ).then((accepted) => {
                if (accepted) setAssistanceAwaitingAcknowledgement(requestId);
              });
            }}
          />}
          {assistance && !assistance.transferOffered && assistanceRemaining > 0 && !lease && <aside className="media-gate assistance-consult"><Mic size={19} /><div><strong>Would you rather speak with Aokie?</strong><p>Start a private consultation. Desktop isolates the caller on software hold before this microphone can open.</p><button className="primary-button" disabled={!consultAllowed || busy || assistanceAwaitingAcknowledgement === assistance.requestId} onClick={() => void run(() => onRequestLease("consult"))}>Consult privately with Aokie</button></div></aside>}
          {call?.secondaryCall && call.remoteCapabilities.secondaryCallObservation === "observed" && (
            <section className="secondary-call-banner" aria-label="Screened secondary caller status">
              <PhoneOff size={19} />
              <div><strong>Another caller was {call.secondaryCall.status}</strong><p>{call.secondaryCall.callbackEligible && call.secondaryCallPolicy === "miss_and_callback" ? "FormLogic callback follow-up is eligible under the current policy." : "No callback eligibility has been proven."}</p></div>
              <span>{call.secondaryCall.stable ? "Verified" : "Changing"}</span>
            </section>
          )}
          <V2Participants
            grants={snapshot.grants}
            consentCurrent={currentConsent}
            participants={call?.participants ?? []}
            audioLevels={call?.audioLevels}
            nativeLevels={exactNativeLevels}
            microphoneActive={Boolean(exactNativeSession && nativeMediaState?.microphoneActive && !authoritativeMicrophoneMuted)}
            localRtcSessionId={lease?.session.rtcSessionId}
          />
          <section className="runtime-captions"><div className="section-title-line"><h2>Live captions</h2><span>Protocol v2</span></div>{!currentConsent || !call?.remoteConsent.captionsEnabled ? <p className="runtime-muted">Current remote consent does not permit captions.</p> : !grants.has("captions_read") ? <p className="runtime-muted">Caption access is not granted for this device.</p> : call?.captions?.length ? call.captions.map((caption) => <article key={caption.captionId}><strong>{caption.speaker}</strong><p>{caption.text}</p><time>{new Date(caption.occurredAt).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}</time></article>) : <p className="runtime-muted">No permission-filtered captions have arrived.</p>}</section>
          {!runtime.mediaBridge && <aside className="media-gate"><AlertTriangle size={19} /><div><strong>Native voice bridge is unavailable</strong><p>Lease controls remain locked because this runtime cannot prove native WebRTC/audio readiness.</p></div></aside>}
          {endCallerSubmitted && <div className="setup-result is-warning" role="status"><RefreshCw className="spin" size={18} /><span>End request accepted. Waiting for Desktop to confirm the cellular call has ended.</span></div>}
          {endCallerCompleted && <div className="setup-result is-success" role="status"><CheckCircle2 size={18} /><span>Desktop confirmed that the caller call ended.</span></div>}
          {call?.serviceMode !== "ended" && <section className="runtime-actions">
            {!lease && <button disabled={!monitorAllowed || busy} onClick={() => void run(() => onRequestLease("monitor"))}><Headphones size={19} /><span>Listen only</span></button>}
            {!lease && assistance && !assistance.transferOffered && <button disabled={!consultAllowed || busy || assistanceAwaitingAcknowledgement === assistance.requestId} onClick={() => void run(() => onRequestLease("consult"))}><Mic size={19} /><span>Private consult</span></button>}
            {!lease && <button disabled={!takeoverAllowed || call?.serviceMode !== "aokie_active" || busy} onClick={() => { setAcceptedTransferRequestId(null); setTakeoverConfirmationOpen(true); }}><Mic size={19} /><span>Take over</span></button>}
            {lease && <button className="return-action" disabled={!leaseRevokeAllowed || busy} onClick={() => void run(() => onRevokeLease(lease.mode === "takeover" ? "operator_return" : "operator_stop"))}><Bot size={19} /><span>{v2LeaseExitLabel(lease.mode)}</span></button>}
            {(lease?.mode === "takeover" || lease?.mode === "consult") && <button disabled={!canToggleMicrophone || busy || autoArmInFlight || microphoneMutation !== null} onClick={() => {
              const muted = !authoritativeMicrophoneMuted;
              setMicrophoneMutation(muted ? "muting" : "unmuting");
              void run(() => onSetMicrophoneMuted(muted), "microphone")
                .finally(() => setMicrophoneMutation(null));
            }}>
              {authoritativeMicrophoneMuted ? <Mic size={19} /> : <MicOff size={19} />}
              <span>{autoArmInFlight ? "Arming microphone…" : microphoneMutation === "muting" ? "Muting microphone…" : microphoneMutation === "unmuting" ? "Proving microphone…" : authoritativeMicrophoneMuted ? "Unmute microphone" : "Mute microphone"}</span>
            </button>}
            {activeTakeoverOwned && <button className="end-action" disabled={busy || endConfirmationPending || endCallerSubmitted || endCallerCompleted} onClick={() => currentEndChallenge ? setEndConfirmationOpen(true) : void run(onPrepareEndCaller, "end_caller", "The hang-up confirmation could not be prepared. The caller remains connected.")}><PhoneOff size={19} /><span>{endCallerSubmitted || endConfirmationPending ? "Hanging up…" : currentEndChallenge ? "Review hang up" : "Hang up"}</span></button>}
          </section>}
          {takeoverConfirmationOpen && (
            <V2TakeoverConfirmation
              busy={busy}
              confirmationMode={takeoverConfirmationMode(runtime.platform)}
              onCancel={() => { setTakeoverConfirmationOpen(false); setAcceptedTransferRequestId(null); }}
              onConfirm={async () => {
                setTakeoverConfirmationOpen(false);
                const generation = onBeginTakeover();
                const selectedTakeoverOffer = acceptedTransferRequestId ? transferOffer : takeoverOffer;
                const selectedTakeoverAllowed = acceptedTransferRequestId ? transferAllowed : takeoverAllowed;
                if (!selectedTakeoverAllowed || !selectedTakeoverOffer ||
                    selectedTakeoverOffer.offer.acceptedTransferRequestId !== (acceptedTransferRequestId ?? undefined)) {
                  onReportFailure("The signed takeover offer expired or was replaced. Wait for refreshed call state and try again.", "takeover");
                  setAcceptedTransferRequestId(null);
                  return;
                }
                const target: ConfirmedTakeoverTarget = {
                  appId: selectedTakeoverOffer.offer.appId,
                  callId: selectedTakeoverOffer.offer.callId,
                  callEpoch: selectedTakeoverOffer.offer.callEpoch,
                  ownerEpoch: selectedTakeoverOffer.offer.ownerEpoch,
                  switchboardRevision: selectedTakeoverOffer.offer.switchboardRevision,
                  remoteRevision: selectedTakeoverOffer.offer.remoteRevision,
                  targetDeviceId: selectedTakeoverOffer.offer.targetDeviceId,
                  requiredConsentPolicyId: selectedTakeoverOffer.offer.requiredConsentPolicyId,
                  requiredConsentPolicyVersion: selectedTakeoverOffer.offer.requiredConsentPolicyVersion,
                  requiredGrants: [...selectedTakeoverOffer.offer.requiredGrants],
                };
                onConfirmTakeoverTarget(generation, target);
                const accepted = await run(
                  () => onRequestLease("takeover", acceptedTransferRequestId ?? undefined),
                  "takeover",
                  "The signed takeover request could not be submitted",
                );
                setAcceptedTransferRequestId(null);
                if (!accepted) return;
                const authoritative = latestSnapshot.current;
                const currentLease = latestLease.current;
                const stillCurrent = authoritative?.appId === target.appId
                  && authoritative.snapshot.callId === target.callId
                  && authoritative.snapshot.callEpoch === target.callEpoch
                  && authoritative.snapshot.ownerEpoch >= target.ownerEpoch;
                const leaseAlreadyPublished = currentLease?.mode === "takeover"
                  && (currentLease.provisional || currentLease.phase === "prepared" || currentLease.phase === "active");
                if (stillCurrent && !leaseAlreadyPublished) onTakeoverEnqueued(generation);
              }}
            />
          )}
          {endConfirmationOpen && currentEndChallenge && (
            <V2EndCallerConfirmation
              callerLabel={call?.caller?.label ?? "this caller"}
              remainingSeconds={Math.max(0, currentEndChallenge.expiresAt - nowSeconds)}
              busy={busy || endConfirmationPending}
              onCancel={() => setEndConfirmationOpen(false)}
              onConfirm={() => confirmEndCaller(currentEndChallenge)}
            />
          )}
        </>
      )}
    </main>
  );
}

function V2ProgressList({ stages }: { stages: V2ProgressStage[] }) {
  return (
    <div className="stepper-list">
      {stages.map((stage, index) => (
        <div className={`stepper-row is-${stage.status}`} key={stage.title}>
          <span>{stage.status === "done" ? <CheckCircle2 size={15} /> : stage.status === "active" ? <RefreshCw className="spin" size={15} /> : index + 1}</span>
          <div><strong>{stage.title}</strong><small>{stage.detail}</small></div>
        </div>
      ))}
    </div>
  );
}

function V2RouteProgressPanel({ mode, serviceMode, leasePhase, remoteAudioReady, finalRouteProven }: {
  mode: "consult" | "takeover";
  serviceMode: V2CallSnapshotEvent["snapshot"]["serviceMode"] | undefined;
  leasePhase: V2LeaseEvent["phase"] | null;
  remoteAudioReady: boolean;
  finalRouteProven: boolean;
}) {
  return (
    <section className="transition-panel v2-route-progress" aria-label={`${mode} connection progress`}>
      <div className="transition-orb"><ShieldCheck size={27} /></div>
      <span className="section-kicker">{mode === "consult" ? "PRIVATE CONSULT" : "SECURE TAKEOVER"}</span>
      <h2>{mode === "consult" ? "Connecting you privately to Aokie" : "Connecting your caller audio"}</h2>
      <p>Your microphone remains blocked until every current server and native-media fact agrees.</p>
      <V2ProgressList stages={v2RouteProgressStages(mode, serviceMode, leasePhase, remoteAudioReady, finalRouteProven)} />
    </section>
  );
}

function V2RecoveryPanel({ transport, lease, nativeMediaState }: {
  transport: V2LiveRuntimeProps["transport"];
  lease: V2LeaseEvent | null;
  nativeMediaState: NativeMediaStateEvent | null;
}) {
  return (
    <section className="recovery-panel v2-recovery-panel" aria-label="Automatic recovery status">
      <div className="transition-orb"><WifiOff size={27} /></div>
      <span className="section-kicker">AUTOMATIC RECOVERY</span>
      <h2>Reconnecting your secure audio</h2>
      <p>The old route is never treated as current. Aokie Desktop keeps ownership decisions authoritative while Companion reconciles.</p>
      <V2ProgressList stages={v2RecoveryProgressStages(transport, Boolean(lease), Boolean(nativeMediaState?.microphoneActive))} />
      <small>Delayed signalling or audio from the previous lease cannot complete recovery.</small>
    </section>
  );
}

function V2UnconfirmedCallPanel({ transport }: { transport: V2LiveRuntimeProps["transport"] }) {
  return (
    <section className="unknown-panel v2-unconfirmed-panel" aria-label="Unconfirmed call state">
      <WifiOff size={34} />
      <h2>Live controls are locked</h2>
      <p>The last call snapshot is context only while signalling is {humanizeV2(transport)}. Companion will wait for a newer authenticated state before enabling any media action.</p>
      <div className="unknown-diagnostics"><span><ShieldCheck size={15} /> No current remote route assumed</span><strong>{humanizeV2(transport)}</strong></div>
    </section>
  );
}

function V2CallEndedPanel({ onOpenCalls, callRecordId }: { onOpenCalls(): void; callRecordId: string | null }) {
  return (
    <section className="ended-panel v2-call-ended-panel" aria-label="Call ended">
      <span className="ended-icon"><CheckCircle2 size={25} /></span>
      <h2>Call complete</h2>
      <p>All Companion media is closed. A role-visible FormLogic call record will appear in Calls after the server publishes it.</p>
      <button className="primary-button" onClick={onOpenCalls}><History size={17} />{callRecordId ? "View this call record" : "Open Calls"}</button>
    </section>
  );
}

function V2FailureNotice({ failure, onDismiss }: { failure: V2RuntimeFailure; onDismiss(): void }) {
  const title: Record<V2RuntimeFailureOperation, string> = {
    realtime: "Realtime request failed",
    request: "Companion request failed",
    takeover: "Takeover did not start",
    microphone: "Microphone was not armed",
    end_caller: "Caller call remains connected",
  };
  return (
    <div className="setup-result is-failed v2-runtime-failure" role="alert">
      <AlertTriangle size={19} />
      <div>
        <strong>{title[failure.operation]}</strong>
        <small>{failure.message}</small>
        <time>{new Date(failure.occurredAt).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })}</time>
      </div>
      <button type="button" onClick={onDismiss} aria-label="Dismiss Companion error">Dismiss</button>
    </div>
  );
}

function V2TakeoverPendingNotice() {
  return (
    <div className="setup-result is-warning v2-runtime-failure v2-takeover-pending" role="status" aria-live="polite">
      <RefreshCw className="spin" size={19} />
      <div>
        <strong>Takeover request accepted</strong>
        <small>Waiting for Aokie Desktop to place the caller on safe software hold and publish the talk lease.</small>
      </div>
    </div>
  );
}

function V2AssistanceAcceptedNotice({ onDismiss }: { onDismiss(): void }) {
  return (
    <div className="setup-result is-success v2-runtime-failure v2-assistance-accepted" role="status" aria-live="polite">
      <CheckCircle2 size={19} />
      <div>
        <strong>Private answer delivered to Aokie</strong>
        <small>The gateway accepted the one-use answer for the current help request. No microphone route was opened.</small>
      </div>
      <button type="button" onClick={onDismiss} aria-label="Dismiss assistance delivery confirmation">Dismiss</button>
    </div>
  );
}

function V2AssistanceRequestCard({ assistance, transferTranscript, remainingSeconds, answer, available, busy, awaitingAcknowledgement, transferAvailable, onAnswerChange, onSubmit, onAcceptTransfer, onDeclineTransfer }: {
  assistance: V2AssistanceRequestEvent;
  transferTranscript: ReturnType<typeof v2TransferTranscript>;
  remainingSeconds: number;
  answer: string;
  available: boolean;
  busy: boolean;
  awaitingAcknowledgement: boolean;
  transferAvailable: boolean;
  onAnswerChange(value: string): void;
  onSubmit(): void;
  onAcceptTransfer(): void;
  onDeclineTransfer(): void;
}) {
  const expired = remainingSeconds <= 0;
  const enabled = available && !expired;
  const urgency = expired ? "is-expired" : remainingSeconds <= 10 ? "is-urgent" : "";
  const countdown = formatAssistanceCountdown(remainingSeconds);
  return (
    <section className={`runtime-assistance ${urgency}`} aria-labelledby="v2-assistance-title">
      <header className="assistance-header">
        <span className="assistance-aokie-mark"><Sparkles size={21} /></span>
        <div>
          <span className="section-kicker">{assistance.transferOffered ? "CALL TRANSFER FROM AOKIE" : "PRIVATE REQUEST FROM AOKIE"}</span>
          <h2 id="v2-assistance-title">{assistance.transferOffered ? "Aokie would like to transfer this caller" : "Aokie needs your confirmation"}</h2>
          <p>{assistance.transferOffered ? "Accepting starts the exact signed takeover path; declining returns a private response to Aokie." : "Your response goes privately to Aokie, not to the caller."}</p>
        </div>
        <span className={`assistance-countdown ${urgency}`} aria-label={expired ? "Request expired" : `${remainingSeconds} seconds remaining`}>
          <Clock3 size={15} />
          <span><strong>{countdown}</strong><small>{expired ? "expired" : "remaining"}</small></span>
        </span>
      </header>
      <article className="assistance-question">
        <div className="assistance-question-label"><MessageSquareText size={16} /><span>{assistance.transferOffered ? "Transfer request" : "Question from Aokie"}</span></div>
        <p>{assistance.question}</p>
        {assistance.context && <aside className="assistance-context"><span>Call context</span><small>{assistance.context}</small></aside>}
      </article>
      {assistance.transferOffered && (
        <section className="transfer-transcript" aria-label="Authenticated call transcript before transfer">
          <div className="assistance-question-label"><MessageSquareText size={16} /><span>Call transcript so far</span></div>
          {transferTranscript.captions.length ? (
            <div className="transfer-transcript-turns">
              {transferTranscript.captions.map((caption) => (
                <article key={caption.captionId}>
                  <strong>{caption.speaker}</strong>
                  <p>{caption.text}</p>
                </article>
              ))}
            </div>
          ) : (
            <p className="runtime-muted"><LockKeyhole size={14} />{transferTranscript.reason}</p>
          )}
        </section>
      )}
      {enabled ? (
        <>
          <label className="assistance-answer-field" htmlFor="v2-assistance-answer">
            <span><strong>{assistance.transferOffered ? "Optional message for Aokie to relay to the caller" : "Your private answer"}</strong><small>{answer.length.toLocaleString()} / {MAX_ASSISTANCE_ANSWER_CHARACTERS.toLocaleString()}</small></span>
            <textarea
              id="v2-assistance-answer"
              rows={3}
              value={answer}
              maxLength={MAX_ASSISTANCE_ANSWER_CHARACTERS}
              disabled={awaitingAcknowledgement}
              placeholder={assistance.transferOffered ? "Optional caller-facing message…" : "Type the confirmation or detail Aokie needs…"}
              onChange={(event) => onAnswerChange(event.target.value)}
            />
          </label>
          {assistance.transferOffered ? (
            <div className="assistance-submit-row">
              <button className="primary-button" disabled={busy || awaitingAcknowledgement || !transferAvailable} onClick={onAcceptTransfer}><PhoneCall size={17} /> Accept transfer</button>
              <button className="secondary-button" disabled={busy || awaitingAcknowledgement} onClick={onDeclineTransfer}>
                {awaitingAcknowledgement ? <RefreshCw className="spin" size={17} /> : <PhoneOff size={17} />}
                {awaitingAcknowledgement ? "Sending decline…" : "Decline"}
              </button>
              <p><ShieldCheck size={14} />The caller moves only after Desktop proves the accepted transfer's active talk lease.</p>
            </div>
          ) : (
            <div className="assistance-submit-row">
              <button className="primary-button" disabled={busy || awaitingAcknowledgement || !answer.trim()} onClick={onSubmit}>
                {awaitingAcknowledgement ? <RefreshCw className="spin" size={17} /> : <Send size={17} />}
                {awaitingAcknowledgement ? "Waiting for Aokie…" : "Send answer privately"}
              </button>
              <p><ShieldCheck size={14} />{awaitingAcknowledgement ? "Sent once. Waiting for authenticated acknowledgement." : "Typed signalling only. Your microphone stays off."}</p>
            </div>
          )}
        </>
      ) : (
        <div className="assistance-locked" role="status">
          {expired ? <Clock3 size={18} /> : <LockKeyhole size={18} />}
          <div><strong>{expired ? "This request has expired" : "Reply is currently locked"}</strong><p>{expired ? "Wait for Aokie to send a fresh question before replying." : "Consent, admission, or the signed request is no longer current."}</p></div>
        </div>
      )}
    </section>
  );
}

export function v2ParticipantAccess(grants: readonly V2Grant[], consentCurrent: boolean) {
  return {
    roster: consentCurrent && grants.includes("participants_read"),
    identity: consentCurrent && grants.includes("participant_identity_read"),
    levels: consentCurrent && grants.includes("audio_levels_read"),
  };
}

function V2Participants({ grants, consentCurrent, participants, audioLevels, nativeLevels, microphoneActive, localRtcSessionId }: {
  grants: readonly V2Grant[];
  consentCurrent: boolean;
  participants: V2CallSnapshotEvent["snapshot"]["participants"];
  audioLevels: V2CallSnapshotEvent["snapshot"]["audioLevels"];
  nativeLevels: NativeMediaLevelsEvent | null;
  microphoneActive: boolean;
  localRtcSessionId?: string;
}) {
  const access = v2ParticipantAccess(grants, consentCurrent);
  const visibleParticipants = access.roster ? participants : [];
  const participantLevels = new Map(
    (access.levels ? audioLevels ?? [] : [])
      .filter((level) => level.source === "companion" && level.participantId)
      .map((level) => [level.participantId as string, level.levelPermille]),
  );
  const nativeMode = access.levels ? nativeLevels?.session.mode : undefined;
  const nativeRemoteLevel = access.levels ? nativeLevels?.remoteLevelPermille : undefined;
  const callerLevel = access.levels ? audioLevels?.find((level) => level.source === "caller" && !level.participantId)?.levelPermille
    ?? (nativeMode === "monitor" || nativeMode === "talk" ? nativeRemoteLevel : undefined) : undefined;
  const aokieLevel = access.levels ? audioLevels?.find((level) => level.source === "aokie" && !level.participantId)?.levelPermille
    ?? (nativeMode === "consult" ? nativeRemoteLevel : undefined) : undefined;
  const localMicrophoneLevel = access.levels && microphoneActive ? nativeLevels?.microphoneLevelPermille : undefined;
  const localParticipantPresent = Boolean(
    localRtcSessionId && visibleParticipants.some((participant) => participant.participantId === localRtcSessionId),
  );
  if (localRtcSessionId) {
    if (localMicrophoneLevel !== undefined) participantLevels.set(localRtcSessionId, localMicrophoneLevel);
    else if (!microphoneActive) participantLevels.delete(localRtcSessionId);
  }
  const hasReportedLevel = callerLevel !== undefined || aokieLevel !== undefined ||
    localMicrophoneLevel !== undefined || participantLevels.size > 0;
  return (
    <section className="runtime-participants" aria-label="Live participants and reported audio levels">
      <div className="section-title-line"><h2><Users size={17} /> Live participants</h2><span>{access.roster ? `${visibleParticipants.length} connected or prepared` : "Roster access not granted"}</span></div>
      <div className="participant-level-list">
        {access.levels && <AudioLevelRow label="Caller" detail="Cellular caller through Aokie Desktop" level={callerLevel} />}
        {access.levels && <AudioLevelRow label="Aokie" detail="Local receptionist audio" level={aokieLevel} />}
        {access.levels && nativeMode && nativeMode !== "monitor" && !nativeMode.startsWith("prepared_") && !localParticipantPresent && (
          <AudioLevelRow
            label="This Companion"
            detail={microphoneActive ? "Your native microphone" : "Microphone muted — no outbound audio"}
            level={localMicrophoneLevel}
          />
        )}
        {visibleParticipants.map((participant) => (
          <AudioLevelRow
            key={participant.participantId}
            label={access.identity ? participant.displayLabel ?? "Enrolled Companion" : "Companion endpoint"}
            detail={participant.participantId === localRtcSessionId && !microphoneActive
              ? `${humanizeV2(participant.mode)} · ${humanizeV2(participant.state)} · microphone muted`
              : `${humanizeV2(participant.mode)} · ${humanizeV2(participant.state)}`}
            level={access.levels ? participantLevels.get(participant.participantId) : undefined}
            meterAllowed={access.levels}
          />
        ))}
      </div>
      {!consentCurrent && <p className="runtime-muted"><LockKeyhole size={14} /> Current remote consent does not permit participant or audio-level reporting.</p>}
      {consentCurrent && !access.roster && <p className="runtime-muted"><LockKeyhole size={14} /> Participant roster access was not granted for this admission.</p>}
      {consentCurrent && !access.levels && <p className="runtime-muted"><LockKeyhole size={14} /> Audio-level access was not granted for this admission.</p>}
      {access.levels && !hasReportedLevel && <p className="runtime-muted"><Volume2 size={14} /> Waiting for real microphone or caller samples; Companion does not invent audio activity.</p>}
    </section>
  );
}

function AudioLevelRow({ label, detail, level, meterAllowed = true }: { label: string; detail: string; level?: number; meterAllowed?: boolean }) {
  const bounded = level === undefined ? null : Math.min(1_000, Math.max(0, level));
  return (
    <div className="participant-level-row">
      <span className="participant-level-avatar">{initials(label)}</span>
      <span className="participant-level-copy"><strong>{label}</strong><small>{detail}</small></span>
      {meterAllowed && <span className={`participant-meter ${bounded === null ? "is-unreported" : ""}`} aria-label={bounded === null ? `${label} audio level not reported` : `${label} audio level ${Math.round(bounded / 10)} percent`}>
        <i style={{ width: `${bounded === null ? 0 : bounded / 10}%` }} />
      </span>}
    </div>
  );
}

function humanizeV2(value: string): string {
  return value.replace(/_/g, " ").replace(/\b\w/g, (letter) => letter.toUpperCase());
}

interface V2SafetyPresentation {
  title: string;
  detail: string;
}

export function v2SafetyPresentation(
  transport: V2LiveRuntimeProps["transport"],
  serviceMode: V2CallSnapshotEvent["snapshot"]["serviceMode"] | undefined,
  lease: V2LeaseEvent | null,
  truthfulMonitorProof: boolean,
  truthfulTalkProof: boolean,
  truthfulConsultProof: boolean,
  truthfulTalkMuted = false,
): V2SafetyPresentation {
  if (transport !== "connected") {
    return {
      title: "CONNECTION LOST — AUDIO BLOCKED",
      detail: "No microphone route is trusted until fresh admission, call state, and media proof agree.",
    };
  }
  if (serviceMode === "ended") return { title: "CALL ENDED", detail: "All Companion media routes are closed." };
  if (serviceMode === "recovering") {
    return { title: "CONNECTION LOST — CALLER SAFELY HELD", detail: "The old lease is revoked while Aokie Desktop restores a safe local route." };
  }
  if (serviceMode === "returning_to_aokie") {
    return { title: "CALLER ON HOLD — RETURNING TO AOKIE", detail: "Your microphone is blocked while Desktop proves Aokie owns the caller route again." };
  }
  if (lease?.mode === "monitor") {
    return truthfulMonitorProof
      ? { title: "LISTENING — YOU CANNOT BE HEARD", detail: "This is a proven receive-only WebRTC route. Companion has not requested a microphone track." }
      : { title: "CONNECTING LISTEN-ONLY — YOU CANNOT BE HEARD", detail: "The microphone route does not exist; caller audio is not claimed ready until native proof arrives." };
  }
  if (lease?.mode === "consult") {
    if (truthfulConsultProof) {
      return { title: "PRIVATE WITH AOKIE — CALLER CANNOT HEAR YOU", detail: "Desktop proves the caller is isolated on software hold; your microphone reaches only Aokie." };
    }
    return { title: "CALLER ON HOLD — MICROPHONE BLOCKED", detail: "Private consultation is not live until the exact native media proof is current." };
  }
  if (lease?.mode === "takeover") {
    if (truthfulTalkProof) {
      return { title: "YOU ARE LIVE — CALLER CAN HEAR YOU", detail: "Desktop and this endpoint prove the same active talk lease, route, and microphone." };
    }
    if (truthfulTalkMuted) {
      return { title: "YOU OWN THE CALL — MICROPHONE MUTED", detail: "Caller audio remains live to this endpoint, but no microphone PCM is being sent." };
    }
    return { title: "CONNECTING — YOU ARE NOT LIVE", detail: "The caller is safely held while the exact Desktop and native media proofs converge." };
  }
  if (serviceMode === "human_active" || serviceMode === "human_pending") {
    return { title: "REMOTE AUDIO LOCKED — WAITING FOR CURRENT LEASE", detail: "The UI will not infer a speaking route from call state alone." };
  }
  if (serviceMode === "consult_active" || serviceMode === "consult_pending" || serviceMode === "soft_hold") {
    return { title: "CALLER ON HOLD — YOU ARE NOT LIVE", detail: "The Desktop is holding the caller, but this endpoint has no current proven media lease." };
  }
  return {
    title: "AOKIE IS HANDLING THE CALL",
    detail: "Companion is an independent microphone and speaker endpoint; it never connects to the Bluetooth dongle.",
  };
}

export function isCurrentEndCallerChallenge(
  event: V2EndCallerEvent | null,
  snapshot: V2CallSnapshotEvent | null,
  lease: V2LeaseEvent | null,
  nowSeconds = Math.floor(Date.now() / 1_000),
): event is V2EndCallerChallengeEvent {
  if (!event || event.kind !== "end_caller_challenge" || !snapshot || !lease) return false;
  const call = snapshot.snapshot;
  return event.expiresAt > nowSeconds &&
    snapshot.grants.includes("end_caller") && snapshot.grants.includes("takeover") &&
    call.remoteConsent.enabled && call.remoteConsent.acknowledged && call.remoteConsent.takeoverEnabled &&
    call.telephonyState === "active" && call.serviceMode === "human_active" &&
    lease.mode === "takeover" && lease.phase === "active" && lease.session.fence > 0 &&
    isCurrent(lease.session.expiresAt) &&
    event.appId === snapshot.appId && event.appId === lease.session.appId &&
    event.deviceId === lease.session.deviceId &&
    event.callId === call.callId && event.callId === lease.session.callId &&
    event.callEpoch === call.callEpoch && event.callEpoch === lease.session.callEpoch &&
    event.ownerEpoch === call.ownerEpoch && event.ownerEpoch === lease.session.ownerEpoch &&
    event.switchboardRevision === call.switchboardRevision &&
    event.remoteRevision === call.remoteRevision &&
    event.leaseId === lease.session.leaseId && event.fence === lease.session.fence;
}

function V2EndCallerConfirmation({ callerLabel, remainingSeconds, busy, onCancel, onConfirm }: {
  callerLabel: string;
  remainingSeconds: number;
  busy: boolean;
  onCancel(): void;
  onConfirm(): Promise<void>;
}) {
  return (
    <div className="modal-layer is-centered live-end-confirmation" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget && !busy) onCancel(); }}>
      <section className="modal-sheet is-dialog" role="dialog" aria-modal="true" aria-labelledby="v2-end-caller-title" aria-describedby="v2-end-caller-description">
        <div className="danger-modal-icon"><PhoneOff size={25} /></div>
        <h2 id="v2-end-caller-title">Hang up the caller?</h2>
        <p className="modal-lead" id="v2-end-caller-description">This tells Aokie Desktop to end the existing cellular call with {callerLabel}. It cannot be undone.</p>
        <div className="end-caller-safety-list">
          <span><AlertTriangle size={16} /> This is different from Return to Aokie</span>
          <span><Bot size={16} /> Return to Aokie keeps the caller connected</span>
          <span><LockKeyhole size={16} /> Confirmation expires in {remainingSeconds} second{remainingSeconds === 1 ? "" : "s"}</span>
        </div>
        <button className="danger-button" disabled={busy || remainingSeconds <= 0} onClick={() => void onConfirm()}><PhoneOff size={18} />{busy ? "Submitting hang-up request…" : "Confirm and hang up"}</button>
        <button className="text-button" disabled={busy} onClick={onCancel}>Keep the call connected</button>
      </section>
    </div>
  );
}

function V2TakeoverConfirmation({ busy, confirmationMode, onCancel, onConfirm }: {
  busy: boolean;
  confirmationMode: TakeoverConfirmationMode;
  onCancel(): void;
  onConfirm(): Promise<void>;
}) {
  const [progress, setProgress] = useState(0);
  const [keyboardArmed, setKeyboardArmed] = useState(false);
  const animationFrame = useRef<number | null>(null);
  const startedAt = useRef<number | null>(null);
  const committed = useRef(false);
  const HOLD_MS = 1_200;
  const usesWindowsTwoStep = confirmationMode === "two_step";

  const cancelHold = useCallback(() => {
    if (animationFrame.current !== null) window.cancelAnimationFrame(animationFrame.current);
    animationFrame.current = null;
    startedAt.current = null;
    if (!committed.current) setProgress(0);
  }, []);

  const commit = useCallback(() => {
    if (committed.current || busy) return;
    committed.current = true;
    cancelHold();
    setProgress(100);
    void onConfirm();
  }, [busy, cancelHold, onConfirm]);

  const startHold = useCallback(() => {
    if (busy || committed.current) return;
    startedAt.current = performance.now();
    const tick = (now: number) => {
      if (startedAt.current === null || committed.current) return;
      const next = Math.min(100, ((now - startedAt.current) / HOLD_MS) * 100);
      setProgress(next);
      if (next >= 100) commit();
      else animationFrame.current = window.requestAnimationFrame(tick);
    };
    animationFrame.current = window.requestAnimationFrame(tick);
  }, [busy, commit]);

  useEffect(() => cancelHold, [cancelHold]);

  return (
    <div className="modal-layer live-takeover-confirmation" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget && !busy) onCancel(); }}>
      <section className="modal-sheet" role="dialog" aria-modal="true" aria-labelledby="v2-takeover-title" aria-describedby="v2-takeover-description">
        <div className="sheet-handle" />
        <div className="takeover-hero-icon"><Mic size={24} /></div>
        <h2 id="v2-takeover-title">Confirm takeover request</h2>
        <p className="modal-lead" id="v2-takeover-description">Opening this window has not sent anything. Aokie Desktop first places the caller on safe software hold; this Companion arms its microphone only after the exact active talk lease and native audio route are proven.</p>
        <div className="takeover-safety-list">
          <span><CheckCircle2 size={16} /> Caller stays on the existing cellular call</span>
          <span><CheckCircle2 size={16} /> Exactly one Companion can speak</span>
          <span><CheckCircle2 size={16} /> Return stops your media and hands the caller back to Aokie</span>
        </div>
        {usesWindowsTwoStep ? (
          <div className="windows-takeover-confirm">
            {!keyboardArmed ? (
              <button className="secondary-button" disabled={busy} onClick={() => setKeyboardArmed(true)}><LockKeyhole size={18} /> Arm takeover confirmation</button>
            ) : (
              <>
                <div className="takeover-armed-state" role="status"><ShieldCheck size={17} /><span><strong>Confirmation armed</strong><small>No request has been sent yet. The next button submits it.</small></span></div>
                <button className="hold-to-confirm windows-confirm-submit" disabled={busy} onClick={commit}><Mic size={18} /><span>{busy ? "Requesting secure takeover…" : "Confirm and request takeover"}</span></button>
              </>
            )}
          </div>
        ) : (
          <>
            <button
              className="hold-to-confirm"
              disabled={busy}
              style={{ "--hold-progress": `${progress}%` } as React.CSSProperties}
              onPointerDown={(event) => { event.currentTarget.setPointerCapture(event.pointerId); startHold(); }}
              onPointerUp={cancelHold}
              onPointerCancel={cancelHold}
              onKeyDown={(event) => { if (event.key === "Escape") onCancel(); }}
            >
              <span className="hold-fill" />
              <Mic size={18} />
              <span>{busy ? "Requesting secure takeover…" : "Press and hold to take over"}</span>
            </button>
            <div className="accessible-confirm">
              <span>Keyboard or assistive technology?</span>
              {!keyboardArmed
                ? <button onClick={() => setKeyboardArmed(true)}>Use two-step confirmation</button>
                : <button className="confirm-link" disabled={busy} onClick={commit}>Confirm secure takeover</button>}
            </div>
          </>
        )}
        <button className="text-button" disabled={busy} onClick={onCancel}>Stay with Aokie</button>
      </section>
    </div>
  );
}

interface LiveRuntimeProps {
  runtime: RuntimeCapabilities;
  state: ReturnType<typeof createCompanionCallState>;
  deviceId: string;
  localMediaProof: LocalMediaProof | null;
  nativeMediaState: NativeMediaStateEvent | null;
  onCommand: SendCommand;
  onToggleMediaMicrophone(session: NativeMediaSession, active: boolean): Promise<void>;
  onBack(): Promise<void>;
}

function LiveRuntime({ runtime, state, deviceId, localMediaProof, nativeMediaState, onCommand, onToggleMediaMicrophone, onBack }: LiveRuntimeProps) {
  const [answer, setAnswer] = useState("");
  const answerRevision = useRef(0);
  const snapshot = state.snapshot;
  const currentAssistanceRequestId = snapshot?.assistanceRequest?.requestId ?? null;
  const currentAssistanceRequestIdRef = useRef<string | null>(currentAssistanceRequestId);
  currentAssistanceRequestIdRef.current = currentAssistanceRequestId;
  const mediaReady = runtime.mediaBridge;
  const connected = state.transport === "connected" && Boolean(snapshot?.gatewayReachable);
  const monitoringAllowed = connected && mediaReady && canMonitor(snapshot);
  const takeoverAllowed = connected && mediaReady && canTakeOver(snapshot) && isCurrent(snapshot?.takeoverOffer?.expiresAt);
  const ownedByThisDevice = snapshot?.talkOwner.kind === "user" && snapshot.talkOwner.deviceId === deviceId;
  const nativeTalkSessionMatches = Boolean(
    snapshot &&
    nativeMediaState?.session.mode === "talk" &&
    nativeMediaState.session.appId === snapshot.appId &&
    nativeMediaState.session.streamNonce === snapshot.streamNonce &&
    nativeMediaState.session.callId === snapshot.callId &&
    nativeMediaState.session.callEpoch === snapshot.callEpoch &&
    nativeMediaState.session.ownerEpoch === snapshot.ownerEpoch &&
    nativeMediaState.session.deviceId === deviceId &&
    snapshot.talkOwner.kind === "user" &&
    nativeMediaState.session.leaseId === snapshot.talkOwner.leaseId &&
    nativeMediaState.session.fence === snapshot.talkOwner.fence &&
    isCurrent(nativeMediaState.session.expiresAt),
  );
  const localMediaActive = Boolean(
    mediaReady &&
    localMediaProof?.active &&
    snapshot &&
    snapshot.talkOwner.kind === "user" &&
    nativeTalkSessionMatches &&
    nativeMediaState?.microphoneActive &&
    nativeMediaState.remoteAudioReady &&
    localMediaProof.appId === snapshot.appId &&
    localMediaProof.streamNonce === snapshot.streamNonce &&
    localMediaProof.rtcSessionId === nativeMediaState?.session.rtcSessionId &&
    localMediaProof.callId === snapshot.callId &&
    localMediaProof.callEpoch === snapshot.callEpoch &&
    localMediaProof.ownerEpoch === snapshot.ownerEpoch &&
    localMediaProof.deviceId === deviceId &&
    localMediaProof.leaseId === snapshot.talkOwner.leaseId &&
    localMediaProof.fence === snapshot.talkOwner.fence &&
    localMediaProof.sdpRevision === nativeMediaState?.session.sdpRevision &&
    localMediaProof.transportGeneration === nativeMediaState?.session.transportGeneration &&
    isCurrent(localMediaProof.expiresAt),
  );
  const returnAllowed = connected && snapshot?.serviceMode === "human_active" && ownedByThisDevice;
  const endAllowed = connected && snapshot?.telephonyState === "active" && snapshot.capabilities.endCaller && isCurrent(snapshot.endConfirmation?.expiresAt);
  const assistanceAllowed = connected && isCurrent(snapshot?.assistanceRequest?.expiresAt);
  const mutationsLocked = areMutatingControlsLocked(state);

  return (
    <main className="live-runtime-root">
      <header className="runtime-header"><button className="icon-button" aria-label="Disconnect" onClick={() => void onBack()}><ArrowLeft size={20} /></button><div><span className="section-kicker">AOKIE COMPANION</span><strong>Live endpoint</strong></div><span className={`transport-pill is-${state.transport}`}>{state.transport === "connected" ? <Wifi size={14} /> : <WifiOff size={14} />}{state.transport}</span></header>
      {runtime.demo && <aside className="media-gate"><AlertTriangle size={19} /><div><strong>Explicit demo connection</strong><p>This state is simulated. No phone, microphone, native media route, or live gateway is connected.</p></div></aside>}
      {!snapshot ? (
        <section className="runtime-empty"><RefreshCw className={state.transport !== "offline" ? "spin" : ""} size={32} /><h1>Waiting for an authoritative call snapshot</h1><p>No call controls are available until the gateway and Aokie Desktop agree on the current call, epochs, and revisions.</p></section>
      ) : (
        <>
          <section className={`runtime-safety is-${snapshot.serviceMode}`}>
            {snapshot.serviceMode === "human_active" ? <Mic size={22} /> : snapshot.serviceMode === "recovering" ? <WifiOff size={22} /> : <Bot size={22} />}
            <div><strong>{safetyTitle(snapshot, connected, deviceId, localMediaActive)}</strong><small>{safetyDetail(snapshot.serviceMode, mediaReady, connected, ownedByThisDevice, localMediaActive)}</small></div>
          </section>
          <section className="runtime-caller"><span>{initials(snapshot.caller?.label)}</span><div><h1>{snapshot.caller?.label ?? "Caller identity hidden"}</h1><p>{snapshot.caller?.maskedNumber ?? "Caller ID permission not granted"}</p></div><em>{snapshot.telephonyState}</em></section>
          <section className="runtime-proof"><span><ShieldCheck size={14} /> Call epoch {snapshot.callEpoch}</span><span><LockKeyhole size={14} /> Owner epoch {snapshot.ownerEpoch}</span><span><RefreshCw size={14} /> Revision {snapshot.remoteRevision}</span></section>
          {nativeMediaState && <section className="runtime-proof" aria-label="Native media state"><span><Radio size={14} /> {nativeMediaState.session.mode} · {nativeMediaState.phase}</span><span><Headphones size={14} /> {nativeMediaState.remoteAudioReady ? "Caller audio ready" : "Waiting for caller audio"}</span><span><Mic size={14} /> {nativeMediaState.microphoneActive ? "Microphone armed" : "Microphone blocked"}</span></section>}
          <section className="runtime-captions"><div className="section-title-line"><h2>Live captions</h2><span>Sequence {snapshot.sequence}</span></div>{!snapshot.capabilities.liveCaptions ? <p className="runtime-muted">Live-caption access is not granted for this device.</p> : snapshot.captions.length ? snapshot.captions.map((caption) => <article key={caption.captionId}><strong>{caption.speaker}</strong><p>{caption.text}</p><time>{new Date(caption.occurredAt).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}</time></article>) : <p className="runtime-muted">No permission-filtered captions have arrived.</p>}</section>
          {!mediaReady && <aside className="media-gate"><AlertTriangle size={19} /><div><strong>Voice bridge is safely locked</strong><p>The realtime client is connected, but this build has no verified native WebRTC/audio plugin. Listen-only and takeover cannot be enabled by UI state alone.</p></div></aside>}
          <section className="runtime-actions">
            <button disabled={!monitoringAllowed || mutationsLocked} onClick={() => void onCommand(snapshot.mediaState === "receiving" ? "monitor_stop" : "monitor_start", {})}><Headphones size={19} /><span>{snapshot.mediaState === "receiving" ? "Stop listening" : "Listen only"}</span></button>
            <button disabled={!takeoverAllowed || mutationsLocked} onClick={() => snapshot.takeoverOffer && void onCommand("takeover_claim", { offerId: snapshot.takeoverOffer.offerId })}><Mic size={19} /><span>Take over</span></button>
            {snapshot.serviceMode === "human_active" && <button className="return-action" disabled={!returnAllowed || mutationsLocked} onClick={() => snapshot.talkOwner.kind === "user" && void onCommand("resume_aokie", { leaseId: snapshot.talkOwner.leaseId, reason: "operator_return" })}><Bot size={19} /><span>Return to Aokie</span></button>}
            {nativeTalkSessionMatches && nativeMediaState && <button disabled={!connected || mutationsLocked} onClick={() => void onToggleMediaMicrophone(nativeMediaState.session, nativeMediaState.microphoneActive)}><Mic size={19} /><span>{nativeMediaState.microphoneActive ? "Mute microphone" : "Unmute microphone"}</span></button>}
            {snapshot.capabilities.endCaller && <button className="end-action" disabled={!endAllowed || mutationsLocked} onClick={() => snapshot.endConfirmation && window.confirm("End the caller's cellular call? Returning to Aokie keeps the call connected.") && void onCommand("end_caller", { confirmationId: snapshot.endConfirmation.confirmationId })}><PhoneOff size={19} /><span>End caller call</span></button>}
          </section>
          <section className="runtime-help">
            <label>
              <span>Private answer to Aokie</span>
              <textarea
                rows={3}
                value={answer}
                maxLength={MAX_ASSISTANCE_ANSWER_CHARACTERS}
                aria-describedby="assistance-answer-limit"
                onChange={(event) => {
                  answerRevision.current += 1;
                  setAnswer(event.target.value);
                }}
                placeholder="Only the minimum answer Aokie needs…"
              />
              <small id="assistance-answer-limit" className="answer-limit">
                {answer.length.toLocaleString()} / {MAX_ASSISTANCE_ANSWER_CHARACTERS.toLocaleString()} characters
              </small>
            </label>
            <button
              className="primary-button"
              disabled={!assistanceAllowed || !answer.trim() || mutationsLocked}
              onClick={() => {
                const assistanceRequest = snapshot.assistanceRequest;
                if (!assistanceRequest) return;
                const submitted = answer.trim();
                const submission = {
                  requestId: assistanceRequest.requestId,
                  editorRevision: answerRevision.current,
                };
                void onCommand("assistance_respond", {
                  requestId: assistanceRequest.requestId,
                  answerId: oneUseId("answer"),
                  answer: submitted,
                }).then((accepted) => {
                  if (shouldClearAssistanceDraft(
                    accepted,
                    submission,
                    currentAssistanceRequestIdRef.current,
                    answerRevision.current,
                  )) {
                    answerRevision.current += 1;
                    setAnswer("");
                  }
                });
              }}
            >
              <Send size={17} /> Send privately
            </button>
          </section>
        </>
      )}
      {state.lastError && <div className="runtime-error" role="alert"><AlertTriangle size={18} /><span>{state.lastError}</span></div>}
    </main>
  );
}

function safetyTitle(
  snapshot: CallSnapshot,
  connected: boolean,
  deviceId: string,
  localMediaActive: boolean,
): string {
  if (snapshot.serviceMode === "ended") return "CALL ENDED";
  if (!connected) {
    return snapshot.talkOwner.kind === "hold"
      ? "CONNECTION LOST · CALLER LAST REPORTED ON HOLD"
      : "CONNECTION LOST · TALK PATH UNCONFIRMED";
  }
  if (snapshot.telephonyState === "ringing") return "CALL RINGING · AOKIE PREPARING";
  switch (snapshot.serviceMode) {
    case "aokie_active": return "AOKIE IS HANDLING THIS CALL";
    case "soft_hold": return "CALLER IS ON SAFE HOLD";
    case "consult_pending": return "UNSUPPORTED CONSULT STATE · CONTROLS LOCKED";
    case "consult_active": return "UNSUPPORTED CONSULT STATE · CONTROLS LOCKED";
    case "human_pending": return "CONNECTING · YOU ARE NOT LIVE";
    case "human_active": {
      const thisDeviceOwnsRoute = snapshot.talkOwner.kind === "user" && snapshot.talkOwner.deviceId === deviceId;
      if (!thisDeviceOwnsRoute) return "ANOTHER OPERATOR IS LIVE TO THE CALLER";
      return localMediaActive
        ? "YOU ARE LIVE TO THE CALLER"
        : "DESKTOP REPORTS YOUR ROUTE · LOCAL MEDIA UNCONFIRMED";
    }
    case "returning_to_aokie": return "RETURNING TO AOKIE · CALLER ON HOLD";
    case "recovering": return "RECOVERING AUDIO · CALLER ON HOLD";
    case "unreachable": return "DESKTOP UNREACHABLE · TALK PATH LOCKED";
  }
}

function safetyDetail(
  mode: string,
  mediaReady: boolean,
  connected: boolean,
  ownedByThisDevice: boolean,
  localMediaActive: boolean,
): string {
  if (!connected) return "All call controls are locked pending a fresh authoritative snapshot";
  if (mode === "human_active" && ownedByThisDevice && !localMediaActive) {
    return "Local lease/media proof is absent · microphone treated as blocked";
  }
  if (!mediaReady) return "Native caller-audio path unavailable · microphone blocked";
  if (mode === "human_active") return "Desktop route and current talk lease confirmed";
  if (mode === "consult_active") return "Isolated consult audio is not implemented · microphone blocked";
  return "Microphone not requested · captions only";
}

function isCurrent(expiresAt?: string | null): boolean {
  return Boolean(expiresAt && Number.isFinite(Date.parse(expiresAt)) && Date.parse(expiresAt) > Date.now());
}

function oneUseId(prefix: string): string {
  return `${prefix}_${crypto.randomUUID()}`;
}

function initials(label?: string | null): string {
  return label?.split(/\s+/).slice(0, 2).map((part) => part[0]).join("").toUpperCase() || "?";
}

function AokieGlyph() {
  return <span className="aokie-mark" aria-hidden="true">{[8, 18, 29, 16, 25, 12, 20].map((height, index) => <i key={index} style={{ height }} />)}</span>;
}

export default App;
