import type { CommandEnvelope } from "../protocol/types";
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

const UNAVAILABLE_MESSAGE = "The native Aokie Companion runtime is not available in this browser.";

export class UnavailableCompanionBridge implements CompanionBridge {
  readonly kind = "unavailable" as const;

  async getRuntimeCapabilities(): Promise<RuntimeCapabilities> {
    return {
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
    throw new Error(UNAVAILABLE_MESSAGE);
  }
  async beginCustomServerAuthorization(_serverUrl: string, _appId?: string): Promise<CustomServerAuthorization> { throw new Error(UNAVAILABLE_MESSAGE); }
  async confirmCustomServerTrust(_authorization: CustomServerAuthorization, _approved: boolean): Promise<{ profileId: string; deviceId: string }> { throw new Error(UNAVAILABLE_MESSAGE); }
  async connectProfile(_profileId: string): Promise<RealtimeConfig> { throw new Error(UNAVAILABLE_MESSAGE); }
  async listServerProfiles(): Promise<ServerProfile[]> { return []; }
  async rotateServerTrust(_profileId: string): Promise<CustomServerAuthorization> { throw new Error(UNAVAILABLE_MESSAGE); }
  async forgetServerProfile(_profileId: string): Promise<ForgetServerProfileResult> { throw new Error(UNAVAILABLE_MESSAGE); }

  async authorizeManaged(_discoveryUrl: string, _deviceId: string, _appId?: string): Promise<RealtimeConfig> {
    throw new Error(UNAVAILABLE_MESSAGE);
  }

  async restoreManaged(): Promise<RealtimeConfig | null> { return null; }
  async forgetManaged(): Promise<void> {}
  async requestNotificationPermission(): Promise<boolean> { return false; }
  async getCompanionBootstrap(): Promise<CompanionBootstrap> { throw new Error(UNAVAILABLE_MESSAGE); }
  async getCompanionHistory(_limit?: number, _before?: number): Promise<CompanionHistory> { throw new Error(UNAVAILABLE_MESSAGE); }
  async getCompanionRouting(): Promise<CompanionRouting> { throw new Error(UNAVAILABLE_MESSAGE); }
  async getCompanionCallRecords(_limit?: number): Promise<CompanionCallRecords> { throw new Error(UNAVAILABLE_MESSAGE); }
  async getCompanionCallRecordDetail(_recordId: string): Promise<CompanionCallRecordDetail> { throw new Error(UNAVAILABLE_MESSAGE); }
  async getCompanionAvailability(): Promise<CompanionAvailability> { throw new Error(UNAVAILABLE_MESSAGE); }
  async setCompanionAvailability(_availability: CompanionAvailabilityState, _expiresInSeconds?: number): Promise<CompanionAvailability> { throw new Error(UNAVAILABLE_MESSAGE); }

  async connect(_config: RealtimeConfig): Promise<void> {
    throw new Error(UNAVAILABLE_MESSAGE);
  }

  async disconnect(): Promise<void> {}

  async send(_command: CommandEnvelope): Promise<void> {
    throw new Error(UNAVAILABLE_MESSAGE);
  }

  async requestV2Lease(_mode: V2LeaseMode, _acceptedTransferRequestId?: string): Promise<V2RequestReceipt> { throw new Error(UNAVAILABLE_MESSAGE); }
  async revokeV2Lease(_reason: string): Promise<V2RequestReceipt> { throw new Error(UNAVAILABLE_MESSAGE); }
  async answerV2Assistance(_requestId: string, _answer: string, _responseAction?: "answer" | "decline"): Promise<{ requestId: string; answerId: string }> { throw new Error(UNAVAILABLE_MESSAGE); }
  async setV2MicrophoneMuted(_muted: boolean): Promise<V2RequestReceipt> { throw new Error(UNAVAILABLE_MESSAGE); }
  async prepareEndCaller(): Promise<V2RequestReceipt> { throw new Error(UNAVAILABLE_MESSAGE); }
  async confirmEndCaller(_confirmationId: string): Promise<V2RequestReceipt> { throw new Error(UNAVAILABLE_MESSAGE); }
  async confirmDesktopPeerTrust(_challenge: DesktopPeerTrustChallenge, _approved: boolean): Promise<void> { throw new Error(UNAVAILABLE_MESSAGE); }
  async reviewDesktopPairingOffer(_profileId: string, _offerJson: string): Promise<DesktopPairingReview> { throw new Error(UNAVAILABLE_MESSAGE); }
  async confirmDesktopPairing(_review: DesktopPairingReview, _confirmation: DesktopPairingConfirmation): Promise<DesktopPairingDecision> { throw new Error(UNAVAILABLE_MESSAGE); }
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
  async selectAudioDevices(_inputId: string, _outputId: string): Promise<NativeAudioDevices> { throw new Error(UNAVAILABLE_MESSAGE); }

  async createMediaOffer(_request: NativeMediaOfferRequest): Promise<NativeMediaOffer> { throw new Error(UNAVAILABLE_MESSAGE); }
  async acceptMediaAnswer(_session: NativeMediaSession, _answer: NativeSdpSignal): Promise<void> { throw new Error(UNAVAILABLE_MESSAGE); }
  async addMediaIceCandidate(_session: NativeMediaSession, _candidate: NativeIceCandidate): Promise<void> { throw new Error(UNAVAILABLE_MESSAGE); }
  async armMediaMicrophone(_session: NativeMediaSession): Promise<void> { throw new Error(UNAVAILABLE_MESSAGE); }
  async disarmMediaMicrophone(_session: NativeMediaSession): Promise<void> { throw new Error(UNAVAILABLE_MESSAGE); }
  async renewMediaLease(_session: NativeMediaSession): Promise<void> { throw new Error(UNAVAILABLE_MESSAGE); }
  async revokeMedia(_session: NativeMediaSession, _reason?: string): Promise<void> { throw new Error(UNAVAILABLE_MESSAGE); }
  async closeMedia(): Promise<void> {}

  subscribe(_listener: (event: BridgeEvent) => void): () => void {
    return () => undefined;
  }
}
