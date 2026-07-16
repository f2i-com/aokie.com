import type { CallSnapshot, CommandAck, CommandEnvelope } from "../protocol/types";

export interface RealtimeConfig {
  gatewayUrl: string;
  appId: string;
  deviceId: string;
  accessToken: string;
  lastSequence?: number;
  lastStreamNonce?: string;
  protocolVersion?: 1 | 2;
  sessionNonce?: string;
  iceServers?: NativeIceServer[];
  relayOnly?: boolean;
  managedDeploymentId?: string;
  managedProfileId?: string;
  localPilot?: boolean;
}

export type NativeMediaMode = "monitor" | "prepared_consult" | "prepared_talk" | "consult" | "talk";

export interface LocalMediaProof {
  appId: string;
  streamNonce: string;
  rtcSessionId: string;
  callId: string;
  callEpoch: number;
  ownerEpoch: number;
  deviceId: string;
  leaseId: string;
  mode: Extract<NativeMediaMode, "consult" | "talk">;
  fence: number;
  sdpRevision: number;
  transportGeneration: number;
  expiresAt: string;
  active: boolean;
}

export interface NativeMediaSession {
  appId: string;
  streamNonce: string;
  rtcSessionId: string;
  callId: string;
  callEpoch: number;
  ownerEpoch: number;
  deviceId: string;
  mode: NativeMediaMode;
  leaseId: string;
  fence: number;
  sdpRevision: number;
  transportGeneration: number;
  expiresAt: string;
}

export interface NativeSdpSignal {
  type: "offer" | "answer";
  sdp: string;
}

export interface NativeIceCandidate {
  sdpMid: string;
  sdpMlineIndex: number;
  candidate: string;
}

export interface NativeIceServer {
  urls: string[];
  username?: string;
  credential?: string;
}

export interface NativeAudioDevice {
  id: string;
  label: string;
}

export interface NativeAudioDevices {
  routingPolicy: "selectable" | "system_managed" | "unavailable";
  inputDevices: NativeAudioDevice[];
  outputDevices: NativeAudioDevice[];
  selectedInputId: string;
  selectedOutputId: string;
  state: "idle" | "media_active" | "microphone_active";
  canSelect: boolean;
}

export interface NativeMediaOfferRequest {
  session: NativeMediaSession;
  iceServers: NativeIceServer[];
  relayOnly: boolean;
}

export interface NativeMediaOffer {
  session: NativeMediaSession;
  offer: NativeSdpSignal;
}

export type NativeMediaSignal =
  | { kind: "offer"; description: NativeSdpSignal }
  | { kind: "ice"; candidate: NativeIceCandidate }
  | { kind: "ice_complete" };

export interface NativeMediaSignalEvent {
  session: NativeMediaSession;
  signal: NativeMediaSignal;
}

export interface NativeMediaStateEvent {
  session: NativeMediaSession;
  phase: string;
  microphoneActive: boolean;
  remoteAudioReady: boolean;
  reason?: string;
}

export interface SyncReady {
  streamNonce: string;
  sequence: number;
}

export interface DesktopPeerTrustChallenge {
  challengeId: string;
  profileId: string;
  appId: string;
  deviceId: string;
  peerFingerprint: string;
  previousPeerFingerprint?: string;
  rotation: boolean;
  expiresAt: number;
}

export interface DesktopPairingReview {
  reviewId: string;
  profileId: string;
  appId: string;
  workspaceId?: string;
  desktopConnectionId: string;
  deviceId: string;
  desktopKeyThumbprint: string;
  desktopFingerprint: string;
  mobileKeyThumbprint: string;
  mobileFingerprint: string;
  issuedAt: number;
  expiresAt: number;
  unsignedPublicOffer: true;
}

export interface DesktopPairingDecision {
  approved: boolean;
  responseJson?: string;
  desktopKeyThumbprint: string;
  mobileKeyThumbprint: string;
  mobileFingerprint: string;
  deviceId: string;
  expiresAt?: number;
}

export interface DesktopPairingConfirmation {
  displayName: string;
  approved: boolean;
  desktopThumbprintConfirmed: boolean;
  mobileFingerprintAcknowledged: boolean;
}

export interface ManagedAdmissionState {
  value: "pairing_required" | "desktop_unavailable" | "policy_denied";
  code: string;
  message: string;
}

export type ServerTrustState = "first_use" | "trusted" | "rotation_required" | "identity_changed";

export interface CustomServerAuthorization {
  authorizationId: string;
  profileId: string;
  origin: string;
  discoveryFingerprint: string;
  endpointFingerprint: string;
  previousDiscoveryFingerprint?: string;
  trustState: ServerTrustState;
  expiresAt: number;
}

export interface ServerProfile {
  profileId: string;
  serverUrl: string;
  origin: string;
  deploymentId: string;
  appId: string;
  deviceId: string;
  discoveryFingerprint: string;
  endpointFingerprint: string;
  trustState: "trusted";
  active: boolean;
}

export interface ForgetServerProfileResult {
  profileId: string;
  forgotten: true;
  remoteCleanup: "completed" | "not_active";
}

export interface DiscoveryDocument {
  schemaVersion: number;
  issuer: string;
  apiBaseUrl: string;
  realtimeUrl?: string;
  gatewayUrl?: string;
  oauthAuthorizationUrl: string;
  oauthTokenUrl: string;
  admissionEndpoint?: string;
  clientId?: string;
  oauthResource?: string;
  deploymentId: string;
  signingKeyId?: string;
  signingKeyFingerprint?: string;
  available: boolean;
  scopesSupported: string[];
  features: string[];
  iceServers: NativeIceServer[];
  relayOnly: boolean;
  turnCredentialExpiresAt: number | null;
  remoteConsent?: {
    configured: boolean;
    remoteMonitoring: boolean;
    remoteConsult: boolean;
    remoteTakeover: boolean;
    remoteCaptions: boolean;
    remoteAssistance: boolean;
  };
  media?: {
    transport: "webrtc";
    gatewayRelaysMedia: false;
    companionUsesBluetoothDongle: false;
    relayOnly: boolean;
  };
  appId?: string;
  appSlug?: string;
  signatureVerified: boolean;
}

export type CompanionCapability =
  | "state_read" | "caller_read" | "captions_read" | "monitor" | "consult"
  | "takeover" | "resume_aokie" | "rtc_signal" | "assistance_read"
  | "assistance_respond" | "end_caller";
export type CompanionAvailabilityState = "available" | "busy" | "offline" | "do_not_disturb";
export type CompanionSessionMode = "monitor" | "consult" | "takeover";

export interface CompanionAvailabilityRecord {
  availability: CompanionAvailabilityState;
  updatedAt: string;
  expiresAt: string | null;
}

export interface CompanionTeamMember {
  staffId: string | null;
  displayName: string;
  roleName: string | null;
  isCurrentUser: boolean;
  priority: number;
  enabled: boolean;
  availability: CompanionAvailabilityState;
  availabilityUpdatedAt: string;
  availabilityExpiresAt: string | null;
}

export interface CompanionStaffMember {
  id: string;
  displayName: string;
  roleName: string;
  isCurrentUser: boolean;
  isOwner: boolean;
  companionReady: boolean;
}

export interface CompanionRoutingGroup {
  id: string;
  name: string;
  policy: "all" | "priority" | "round_robin";
  enabled: boolean;
  members: CompanionTeamMember[];
}

export interface CompanionActivity {
  id: string;
  eventId: string;
  appId: string;
  sessionRecordId: string | null;
  callId: string | null;
  deviceId: string | null;
  actorUserId: string | null;
  subjectId: string;
  eventType:
    | "admission_issued" | "monitor_joined" | "monitor_left" | "consult_joined"
    | "consult_left" | "takeover_prepared" | "takeover_joined" | "takeover_left"
    | "returned_to_aokie" | "session_recovered" | "session_revoked"
    | "endpoint_revoked" | "call_alert_targeted" | "assistance_targeted"
    | "takeover_targeted";
  mode: CompanionSessionMode | null;
  reason: string | null;
  ownerEpoch: number | null;
  occurredAt: string;
}

export interface CompanionSession {
  id: string;
  sessionId: string;
  callId: string;
  deviceId: string | null;
  subjectId: string;
  mode: CompanionSessionMode;
  state: "prepared" | "joined" | "left" | "revoked";
  joinedAt: string | null;
  endedAt: string | null;
  endReason: string | null;
  lastEventId: string;
  lastEventAt: string;
}

export interface CompanionHistory {
  activity: CompanionActivity[];
  sessions: CompanionSession[];
}

export interface CompanionCallRecord {
  id: string;
  callId: string;
  callerName: string | null;
  maskedNumber: string | null;
  status: string;
  direction: "inbound" | "outbound" | "unknown";
  summary: string | null;
  startedAt: string | null;
  endedAt: string | null;
  durationSeconds: number | null;
  followUpRequired: boolean;
  submittedAt: string;
}

export interface CompanionCallRecords {
  records: CompanionCallRecord[];
  access: "full" | "own" | "none";
}

export interface CompanionCallTranscriptTurn {
  id: string;
  speaker: string;
  text: string;
  occurredAt: string;
}

export interface CompanionCallFollowUp {
  id: string;
  summary: string;
  status: string;
  priority: string;
  submittedAt: string;
}

export interface CompanionCallRecordDetail {
  record: CompanionCallRecord;
  transcript: CompanionCallTranscriptTurn[];
  followUps: CompanionCallFollowUp[];
}

export interface CompanionPushEndpoint {
  id: string;
  appId: string;
  deviceId: string;
  kind: "fcm" | "apns" | "apns_voip";
  mode: "managed" | "broker";
  provider: "fcm" | "apns" | "broker";
  environment: "sandbox" | "production";
  topic: string | null;
  fingerprint: string;
  invalidatedAt: string | null;
  rotatedAt: string;
}

export interface CompanionBootstrap {
  membership: { appId: string; appSlug: string; status: "active" };
  device: {
    id: string;
    userId: string;
    appId: string;
    subjectId: string;
    role: "mobile";
    displayName: string;
    grants: CompanionCapability[];
    approvedAt: string;
    lastSeenAt: string;
    revokedAt: null;
  };
  capabilities: CompanionCapability[];
  availability: CompanionAvailabilityRecord | null;
  routingGroups: CompanionRoutingGroup[];
  staff: CompanionStaffMember[];
  history: CompanionHistory;
  pushEndpoints: CompanionPushEndpoint[];
}

export interface CompanionRouting {
  routingGroups: CompanionRoutingGroup[];
  staff: CompanionStaffMember[];
}

export interface CompanionAvailability {
  appId: string;
  deviceId: string;
  availability: CompanionAvailabilityRecord | null;
}

export type V2Grant =
  | "state_read"
  | "caller_read"
  | "captions_read"
  | "assistance_read"
  | "assistance_respond"
  | "monitor"
  | "consult"
  | "takeover"
  | "resume_aokie"
  | "rtc_signal"
  | "end_caller";

export type V2LeaseMode = "monitor" | "consult" | "takeover";
export type V2LeasePhase = "prepared" | "active";

export interface V2PendingMobileOffer {
  offer: {
    offerId: string;
    opportunityId: string;
    targetDeviceId: string;
    targetHolderKeyThumbprint: string;
    offeredMode: V2LeaseMode;
    surface: "in_app" | "voice_system_ui";
    appId: string;
    callId: string;
    callEpoch: number;
    ownerEpoch: number;
    switchboardRevision: number;
    remoteRevision: number;
    requiredConsentPolicyId: string;
    requiredConsentPolicyVersion: number;
    requiredGrants: V2Grant[];
    issuedAt: number;
    expiresAt: number;
    jti: string;
  };
  offerToken: string;
}

export interface V2CallSnapshotEvent {
  kind: "snapshot";
  schemaVersion: 2;
  appId: string;
  sequence: number;
  grants: V2Grant[];
  snapshot: {
    callId: string;
    callEpoch: number;
    ownerEpoch: number;
    switchboardRevision: number;
    remoteRevision: number;
    telephonyState: "ringing" | "active" | "held" | "ending" | "ended";
    serviceMode: "aokie_active" | "soft_hold" | "consult_pending" | "consult_active" | "human_pending" | "human_active" | "returning_to_aokie" | "recovering" | "ended";
    mediaState: "none" | "ready" | "receiving" | "connecting" | "active" | "failed";
    remoteCapabilities: {
      softwareHold: boolean;
      carrierHoldEvidence: "unknown" | "negotiated" | "observed" | "proven";
      secondaryCallObservation: "unknown" | "negotiated" | "observed";
      voiceConsult: boolean;
      takeover: boolean;
    };
    secondaryCallPolicy: "normal" | "miss_and_callback";
    secondaryCall?: {
      stable: boolean;
      callbackEligible: boolean;
      status: "queued" | "attempted" | "failed";
      waitingCallId?: string;
    };
    remoteConsent: {
      policyId: string;
      policyVersion: number;
      enabled: boolean;
      acknowledged: boolean;
      acknowledgedAt?: string;
      expiresAt?: string;
      captionsEnabled: boolean;
      assistanceEnabled: boolean;
      monitorEnabled: boolean;
      consultEnabled: boolean;
      takeoverEnabled: boolean;
    };
    caller?: { label?: string | null; maskedNumber?: string | null } | null;
    captions?: Array<{ captionId: string; speaker: string; text: string; occurredAt: string; finalText: boolean }>;
    participants: Array<{
      participantId: string;
      mode: "observer" | "advisor" | "talker";
      state: "connected" | "prepared" | "active";
      subjectId?: string;
      displayLabel?: string;
    }>;
    audioLevels?: Array<{
      source: "caller" | "aokie" | "companion";
      participantId?: string;
      levelPermille: number;
    }>;
    pendingMobileOffers: V2PendingMobileOffer[];
    occurredAt: string;
  };
}

export interface V2IdleSyncEvent {
  kind: "idle_sync";
  schemaVersion: 2;
  appId: string;
  sequence: number;
  grants: V2Grant[];
}

export interface V2AssistanceRequestEvent {
  kind: "assistance_request";
  schemaVersion: 2;
  appId: string;
  eventId: string;
  requestId: string;
  callId: string;
  callEpoch: number;
  ownerEpoch: number;
  switchboardRevision: number;
  remoteRevision: number;
  question: string;
  context?: string;
  expiresAt: number;
}

export interface V2AssistanceAnswerAcceptedEvent {
  kind: "assistance_answer_accepted";
  schemaVersion: 2;
  appId: string;
  requestId: string;
  answerId: string;
  accepted: true;
}

export interface V2RequestReceipt {
  requestId: string;
}

export interface V2EndCallerChallengeEvent {
  kind: "end_caller_challenge";
  schemaVersion: 2;
  appId: string;
  requestId: string;
  confirmationId: string;
  deviceId: string;
  callId: string;
  callEpoch: number;
  ownerEpoch: number;
  switchboardRevision: number;
  remoteRevision: number;
  leaseId: string;
  fence: number;
  expiresAt: number;
}

export interface V2EndCallerSubmittedEvent {
  kind: "end_caller_submitted";
  schemaVersion: 2;
  appId: string;
  requestId: string;
  operationId: string;
  confirmationId: string;
  accepted: true;
}

export interface V2EndCallerResultEvent {
  kind: "end_caller_result";
  schemaVersion: 2;
  appId: string;
  operationId: string;
  confirmationId: string;
  deviceId: string;
  callId: string;
  callEpoch: number;
  ownerEpoch: number;
  switchboardRevision: number;
  remoteRevision: number;
  leaseId: string;
  fence: number;
  outcome: "completed" | "failed";
  code?: string;
  message?: string;
}

export interface V2EndCallerFailureEvent {
  kind: "end_caller_failure";
  schemaVersion: 2;
  requestId: string;
  code: string;
  message: string;
}

export type V2EndCallerEvent =
  | V2EndCallerChallengeEvent
  | V2EndCallerSubmittedEvent
  | V2EndCallerResultEvent
  | V2EndCallerFailureEvent;

export interface V2LeaseEvent {
  session: NativeMediaSession;
  mode: V2LeaseMode;
  phase: V2LeasePhase;
  provisional: boolean;
}

export interface RuntimeCapabilities {
  platform: string;
  tauri: boolean;
  secureStorage: boolean;
  notifications: boolean;
  microphone: boolean;
  nativeCallUi: boolean;
  realtime: boolean;
  mediaBridge: boolean;
  demo: boolean;
  localPilot: boolean;
  notificationPermission: string;
  microphonePermission: string;
  fcmConfigured: boolean;
  fcmTokenPresent: boolean;
  pushRegistration: string;
  pendingCallOffer: boolean;
  batteryOptimizationsRestricted: boolean;
  forceStopState: string;
  callInfrastructure: string;
  lastNativeDiagnostic: string | null;
}

export type BridgeEvent =
  | { type: "transport"; value: "connecting" | "connected" | "reconnecting" | "offline" }
  | { type: "snapshot"; value: CallSnapshot }
  | { type: "sync_ready"; value: SyncReady }
  | { type: "local_media"; value: LocalMediaProof | null }
  | { type: "media_signal"; value: NativeMediaSignalEvent }
  | { type: "media_state"; value: NativeMediaStateEvent }
  | { type: "v2_snapshot"; value: V2CallSnapshotEvent }
  | { type: "v2_idle_sync"; value: V2IdleSyncEvent }
  | { type: "v2_lease"; value: V2LeaseEvent | null }
  | { type: "v2_assistance"; value: V2AssistanceRequestEvent | null }
  | { type: "v2_assistance_answered"; value: V2AssistanceAnswerAcceptedEvent }
  | { type: "v2_end_caller"; value: V2EndCallerEvent }
  | { type: "desktop_peer_trust_required"; value: DesktopPeerTrustChallenge }
  | { type: "managed_admission_state"; value: ManagedAdmissionState }
  | { type: "command_ack"; value: CommandAck }
  | { type: "error"; message: string };

export interface CompanionBridge {
  readonly kind: "native" | "demo" | "unavailable";
  getRuntimeCapabilities(): Promise<RuntimeCapabilities>;
  discover(url: string): Promise<DiscoveryDocument>;
  beginCustomServerAuthorization(serverUrl: string, appId?: string): Promise<CustomServerAuthorization>;
  confirmCustomServerTrust(authorization: CustomServerAuthorization, approved: boolean): Promise<{ profileId: string; deviceId: string }>;
  connectProfile(profileId: string): Promise<RealtimeConfig>;
  listServerProfiles(): Promise<ServerProfile[]>;
  rotateServerTrust(profileId: string): Promise<CustomServerAuthorization>;
  forgetServerProfile(profileId: string): Promise<ForgetServerProfileResult>;
  authorizeManaged(discoveryUrl: string, deviceId: string, appId?: string): Promise<RealtimeConfig>;
  restoreManaged(): Promise<RealtimeConfig | null>;
  forgetManaged(): Promise<void>;
  requestNotificationPermission(): Promise<boolean>;
  getCompanionBootstrap(): Promise<CompanionBootstrap>;
  getCompanionHistory(limit?: number, before?: number): Promise<CompanionHistory>;
  getCompanionRouting(): Promise<CompanionRouting>;
  getCompanionCallRecords(limit?: number): Promise<CompanionCallRecords>;
  getCompanionCallRecordDetail(recordId: string): Promise<CompanionCallRecordDetail>;
  getCompanionAvailability(): Promise<CompanionAvailability>;
  setCompanionAvailability(availability: CompanionAvailabilityState, expiresInSeconds?: number): Promise<CompanionAvailability>;
  connect(config: RealtimeConfig): Promise<void>;
  disconnect(): Promise<void>;
  send(command: CommandEnvelope): Promise<void>;
  requestV2Lease(mode: V2LeaseMode): Promise<V2RequestReceipt>;
  revokeV2Lease(reason: string): Promise<V2RequestReceipt>;
  answerV2Assistance(requestId: string, answer: string): Promise<{ requestId: string; answerId: string }>;
  prepareEndCaller(): Promise<V2RequestReceipt>;
  confirmEndCaller(confirmationId: string): Promise<V2RequestReceipt>;
  confirmDesktopPeerTrust(challenge: DesktopPeerTrustChallenge, approved: boolean): Promise<void>;
  reviewDesktopPairingOffer(profileId: string, offerJson: string): Promise<DesktopPairingReview>;
  confirmDesktopPairing(review: DesktopPairingReview, confirmation: DesktopPairingConfirmation): Promise<DesktopPairingDecision>;
  getAudioDevices(): Promise<NativeAudioDevices>;
  selectAudioDevices(inputId: string, outputId: string): Promise<NativeAudioDevices>;
  createMediaOffer(request: NativeMediaOfferRequest): Promise<NativeMediaOffer>;
  acceptMediaAnswer(session: NativeMediaSession, answer: NativeSdpSignal): Promise<void>;
  addMediaIceCandidate(session: NativeMediaSession, candidate: NativeIceCandidate): Promise<void>;
  armMediaMicrophone(session: NativeMediaSession): Promise<void>;
  disarmMediaMicrophone(session: NativeMediaSession): Promise<void>;
  renewMediaLease(session: NativeMediaSession): Promise<void>;
  revokeMedia(session: NativeMediaSession, reason?: string): Promise<void>;
  closeMedia(): Promise<void>;
  subscribe(listener: (event: BridgeEvent) => void): () => void;
}
