import { demoSnapshot } from "../features/demo/snapshot";
import type { CallSnapshot, CommandEnvelope } from "../protocol/types";
import type {
  BridgeEvent,
  CompanionAvailability,
  CompanionAvailabilityState,
  CompanionBridge,
  CompanionBootstrap,
  CompanionCallRecordDetail,
  CompanionCallRecords,
  CustomServerAuthorization,
  CompanionHistory,
  CompanionRouting,
  DesktopPairingConfirmation,
  DesktopPairingDecision,
  DesktopPairingReview,
  DiscoveryDocument,
  DesktopPeerTrustChallenge,
  ForgetServerProfileResult,
  NativeAudioDevices,
  NativeIceCandidate,
  NativeMediaOffer,
  NativeMediaOfferRequest,
  NativeMediaSession,
  NativeSdpSignal,
  RealtimeConfig,
  RuntimeCapabilities,
  ServerProfile,
  V2LeaseMode,
  V2RequestReceipt,
} from "./CompanionBridge";

type Listener = (event: BridgeEvent) => void;

function nextSnapshot(current: CallSnapshot, command: CommandEnvelope): CallSnapshot {
  const base = {
    ...current,
    sequence: current.sequence + 1,
    remoteRevision: current.remoteRevision + 1,
    occurredAt: new Date().toISOString(),
  };
  switch (command.type) {
    case "monitor_start":
      return { ...base, mediaState: "receiving" };
    case "monitor_stop":
      return { ...base, mediaState: "ready" };
    case "resume_aokie":
      return {
        ...base,
        ownerEpoch: current.ownerEpoch + 1,
        serviceMode: "aokie_active",
        talkOwner: { kind: "aokie" },
        mediaState: "ready",
      };
    case "takeover_claim":
      return {
        ...base,
        ownerEpoch: current.ownerEpoch + 1,
        serviceMode: "human_active",
        talkOwner: {
          kind: "user",
          userId: "user_demo",
          deviceId: "device_demo",
          leaseId: "lease_demo_only",
          fence: 1,
        },
        mediaState: "active",
      };
    case "end_caller":
      return {
        ...base,
        telephonyState: "ended",
        serviceMode: "ended",
        talkOwner: { kind: "none" },
        mediaState: "none",
      };
    case "assistance_respond":
    default:
      return base;
  }
}

export class DemoCompanionBridge implements CompanionBridge {
  readonly kind = "demo" as const;
  private listeners = new Set<Listener>();
  private snapshot = structuredClone(demoSnapshot);

  async getRuntimeCapabilities(): Promise<RuntimeCapabilities> {
    return {
      platform: "demo",
      tauri: false,
      secureStorage: false,
      notifications: false,
      microphone: false,
      nativeCallUi: false,
      realtime: true,
      mediaBridge: false,
      demo: true,
      localPilot: false,
      notificationPermission: "not_applicable",
      microphonePermission: "not_applicable",
      fcmConfigured: false,
      fcmTokenPresent: false,
      pushRegistration: "not_applicable",
      pendingCallOffer: false,
      batteryOptimizationsRestricted: false,
      forceStopState: "not_detectable",
      callInfrastructure: "not_applicable",
      lastNativeDiagnostic: null,
    };
  }

  async discover(_url: string): Promise<DiscoveryDocument> {
    return {
      schemaVersion: 1,
      issuer: "demo.aokie.invalid",
      apiBaseUrl: "https://demo.aokie.invalid/api",
      gatewayUrl: "wss://demo.aokie.invalid/v1/realtime",
      realtimeUrl: "wss://demo.aokie.invalid/v1/realtime",
      oauthAuthorizationUrl: "https://demo.aokie.invalid/oauth/authorize",
      oauthTokenUrl: "https://demo.aokie.invalid/oauth/token",
      deploymentId: "demo",
      signingKeyId: "demo-untrusted",
      available: true,
      scopesSupported: [],
      features: [],
      iceServers: [],
      relayOnly: false,
      turnCredentialExpiresAt: null,
      signatureVerified: false,
    };
  }

  async beginCustomServerAuthorization(_serverUrl: string, _appId?: string): Promise<CustomServerAuthorization> { throw new Error("Demo mode cannot authorize a server profile."); }
  async confirmCustomServerTrust(_authorization: CustomServerAuthorization, _approved: boolean): Promise<{ profileId: string; deviceId: string }> { throw new Error("Demo mode cannot trust a server profile."); }
  async connectProfile(_profileId: string): Promise<RealtimeConfig> { throw new Error("Demo mode cannot connect a server profile."); }
  async listServerProfiles(): Promise<ServerProfile[]> { return []; }
  async rotateServerTrust(_profileId: string): Promise<CustomServerAuthorization> { throw new Error("Demo mode cannot rotate server trust."); }
  async forgetServerProfile(_profileId: string): Promise<ForgetServerProfileResult> { throw new Error("Demo mode cannot forget a server profile."); }

  async authorizeManaged(_discoveryUrl: string, _deviceId: string, _appId?: string): Promise<RealtimeConfig> {
    throw new Error("Demo mode cannot authorize a managed deployment.");
  }

  async restoreManaged(): Promise<RealtimeConfig | null> { return null; }
  async forgetManaged(): Promise<void> {}
  async requestNotificationPermission(): Promise<boolean> { return false; }
  async getCompanionBootstrap(): Promise<CompanionBootstrap> { throw new Error("Demo mode has no managed Companion account data."); }
  async getCompanionHistory(_limit?: number, _before?: number): Promise<CompanionHistory> { throw new Error("Demo mode has no managed Companion account data."); }
  async getCompanionRouting(): Promise<CompanionRouting> { throw new Error("Demo mode has no managed Companion account data."); }
  async getCompanionCallRecords(_limit?: number): Promise<CompanionCallRecords> { throw new Error("Demo mode has no managed FormLogic call records."); }
  async getCompanionCallRecordDetail(_recordId: string): Promise<CompanionCallRecordDetail> { throw new Error("Demo mode has no managed FormLogic call records."); }
  async getCompanionAvailability(): Promise<CompanionAvailability> { throw new Error("Demo mode has no managed Companion account data."); }
  async setCompanionAvailability(_availability: CompanionAvailabilityState, _expiresInSeconds?: number): Promise<CompanionAvailability> { throw new Error("Demo mode has no managed Companion account data."); }

  async connect(_config: RealtimeConfig): Promise<void> {
    this.emit({ type: "transport", value: "connected" });
    this.emit({ type: "snapshot", value: structuredClone(this.snapshot) });
  }

  async disconnect(): Promise<void> {
    this.emit({ type: "transport", value: "offline" });
  }

  async send(command: CommandEnvelope): Promise<void> {
    if (command.type === "consult_claim" || command.type === "consult_end") {
      throw new Error("Private voice consult is unavailable until Desktop provides an isolated audio route.");
    }
    this.emit({ type: "command_ack", value: { commandId: command.commandId, accepted: true } });
    this.snapshot = nextSnapshot(this.snapshot, command);
    this.emit({ type: "snapshot", value: structuredClone(this.snapshot) });
  }

  async requestV2Lease(_mode: V2LeaseMode, _acceptedTransferRequestId?: string): Promise<V2RequestReceipt> {
    throw new Error("Demo mode cannot request live media authority.");
  }

  async revokeV2Lease(_reason: string): Promise<V2RequestReceipt> {
    throw new Error("Demo mode has no live media authority.");
  }

  async answerV2Assistance(requestId: string, _answer: string, _responseAction?: "answer" | "decline"): Promise<{ requestId: string; answerId: string }> {
    return { requestId, answerId: `demo_answer_${Date.now()}` };
  }

  async setV2MicrophoneMuted(_muted: boolean): Promise<V2RequestReceipt> {
    throw new Error("Demo mode cannot change live microphone authority.");
  }

  async prepareEndCaller(): Promise<V2RequestReceipt> {
    throw new Error("Demo mode cannot end a live caller call.");
  }

  async confirmEndCaller(_confirmationId: string): Promise<V2RequestReceipt> {
    throw new Error("Demo mode cannot end a live caller call.");
  }

  async confirmDesktopPeerTrust(_challenge: DesktopPeerTrustChallenge, _approved: boolean): Promise<void> {
    throw new Error("Demo mode has no Desktop peer trust ceremony.");
  }
  async reviewDesktopPairingOffer(_profileId: string, _offerJson: string): Promise<DesktopPairingReview> { throw new Error("Demo mode has no Desktop pairing ceremony."); }
  async confirmDesktopPairing(_review: DesktopPairingReview, _confirmation: DesktopPairingConfirmation): Promise<DesktopPairingDecision> { throw new Error("Demo mode has no Desktop pairing ceremony."); }

  async getAudioDevices(): Promise<NativeAudioDevices> {
    return {
      routingPolicy: "unavailable",
      inputDevices: [],
      outputDevices: [],
      selectedInputId: "unavailable",
      selectedOutputId: "unavailable",
      state: "idle",
      canSelect: false,
    };
  }

  async selectAudioDevices(_inputId: string, _outputId: string): Promise<NativeAudioDevices> {
    throw new Error("Demo mode has no native microphone or speaker endpoints.");
  }

  async createMediaOffer(_request: NativeMediaOfferRequest): Promise<NativeMediaOffer> { throw new Error("Demo mode cannot open native media."); }
  async acceptMediaAnswer(_session: NativeMediaSession, _answer: NativeSdpSignal): Promise<void> { throw new Error("Demo mode cannot open native media."); }
  async addMediaIceCandidate(_session: NativeMediaSession, _candidate: NativeIceCandidate): Promise<void> { throw new Error("Demo mode cannot open native media."); }
  async armMediaMicrophone(_session: NativeMediaSession): Promise<void> { throw new Error("Demo mode cannot open native media."); }
  async disarmMediaMicrophone(_session: NativeMediaSession): Promise<void> { throw new Error("Demo mode cannot open native media."); }
  async renewMediaLease(_session: NativeMediaSession): Promise<void> { throw new Error("Demo mode cannot open native media."); }
  async revokeMedia(_session: NativeMediaSession, _reason?: string): Promise<void> { throw new Error("Demo mode cannot open native media."); }
  async closeMedia(): Promise<void> {}

  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  private emit(event: BridgeEvent) {
    for (const listener of this.listeners) listener(event);
  }
}
