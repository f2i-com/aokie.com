export type Screen =
  | "onboarding"
  | "home"
  | "live"
  | "calls"
  | "call-detail"
  | "team"
  | "settings";

export type Scenario =
  | "aokie"
  | "listening"
  | "help"
  | "pending"
  | "human"
  | "recovery"
  | "second-caller"
  | "offline"
  | "revoked"
  | "ended";

export type ServiceMode =
  | "aokie_active"
  | "soft_hold"
  | "human_pending"
  | "human_active"
  | "returning_to_aokie"
  | "recovering"
  | "unreachable"
  | "ended";

export type ViewerMode =
  | "detached"
  | "captions"
  | "monitor"
  | "talker";

export type Availability = "available" | "quiet" | "unavailable";

export type TalkOwner =
  | { kind: "aokie" }
  | { kind: "hold" }
  | { kind: "user"; userId: string; deviceId: string }
  | { kind: "none" };

export interface PrototypeState {
  screen: Screen;
  scenario: Scenario;
  serviceMode: ServiceMode;
  viewerMode: ViewerMode;
  talkOwner: TalkOwner;
  availability: Availability;
  helpOpen: boolean;
  helpAnswer: string;
  helpSent: boolean;
  takeoverOpen: boolean;
  takeoverConfirmed: boolean;
  pendingStep: number;
  recoveryCountdown: number;
  micMuted: boolean;
  audioRoute: "speaker" | "earpiece" | "headset";
  moreOpen: boolean;
  endCallOpen: boolean;
  callEnded: boolean;
  historyFilter: string;
  selectedCallId: string;
  onboardingStep: number;
  deploymentMode: "hosted" | "self-hosted";
  disclosureEnabled: boolean;
  staticMode: boolean;
  toast: string | null;
}

export type PrototypeAction =
  | { type: "NAVIGATE"; screen: Screen }
  | { type: "SET_SCENARIO"; scenario: Scenario }
  | { type: "SET_AVAILABILITY"; availability: Availability }
  | { type: "OPEN_HELP" }
  | { type: "CLOSE_HELP" }
  | { type: "SET_HELP_ANSWER"; value: string }
  | { type: "SEND_HELP" }
  | { type: "OPEN_TAKEOVER" }
  | { type: "CLOSE_TAKEOVER" }
  | { type: "ARM_TAKEOVER" }
  | { type: "START_TAKEOVER" }
  | { type: "ADVANCE_PENDING" }
  | { type: "RETURN_TO_AOKIE" }
  | { type: "FINISH_RETURN" }
  | { type: "RECOVERY_TICK" }
  | { type: "RECOVERY_FALLBACK" }
  | { type: "TOGGLE_MUTE" }
  | { type: "SET_AUDIO_ROUTE"; route: PrototypeState["audioRoute"] }
  | { type: "TOGGLE_MORE" }
  | { type: "OPEN_END_CALL" }
  | { type: "CLOSE_END_CALL" }
  | { type: "END_CALL" }
  | { type: "SET_HISTORY_FILTER"; filter: string }
  | { type: "OPEN_CALL_DETAIL"; callId: string }
  | { type: "SET_ONBOARDING_STEP"; step: number }
  | { type: "SET_DEPLOYMENT"; mode: PrototypeState["deploymentMode"] }
  | { type: "SET_STATIC_MODE"; value: boolean }
  | { type: "TOGGLE_DISCLOSURE" }
  | { type: "CLEAR_TOAST" }
  | { type: "RESET" };

function stateForScenario(scenario: Scenario): Pick<
  PrototypeState,
  "scenario" | "serviceMode" | "viewerMode" | "talkOwner" | "pendingStep" | "recoveryCountdown" | "callEnded"
> {
  switch (scenario) {
    case "listening":
      return {
        scenario,
        serviceMode: "aokie_active",
        viewerMode: "monitor",
        talkOwner: { kind: "aokie" },
        pendingStep: 0,
        recoveryCountdown: 3,
        callEnded: false,
      };
    case "pending":
      return {
        scenario,
        serviceMode: "human_pending",
        viewerMode: "captions",
        talkOwner: { kind: "hold" },
        pendingStep: 1,
        recoveryCountdown: 3,
        callEnded: false,
      };
    case "human":
      return {
        scenario,
        serviceMode: "human_active",
        viewerMode: "talker",
        talkOwner: { kind: "user", userId: "lance", deviceId: "iphone-15" },
        pendingStep: 4,
        recoveryCountdown: 3,
        callEnded: false,
      };
    case "recovery":
      return {
        scenario,
        serviceMode: "recovering",
        viewerMode: "captions",
        talkOwner: { kind: "hold" },
        pendingStep: 0,
        recoveryCountdown: 3,
        callEnded: false,
      };
    case "offline":
      return {
        scenario,
        serviceMode: "unreachable",
        viewerMode: "detached",
        talkOwner: { kind: "none" },
        pendingStep: 0,
        recoveryCountdown: 3,
        callEnded: false,
      };
    case "revoked":
      return {
        scenario,
        serviceMode: "aokie_active",
        viewerMode: "captions",
        talkOwner: { kind: "aokie" },
        pendingStep: 0,
        recoveryCountdown: 3,
        callEnded: false,
      };
    case "ended":
      return {
        scenario,
        serviceMode: "ended",
        viewerMode: "detached",
        talkOwner: { kind: "none" },
        pendingStep: 0,
        recoveryCountdown: 0,
        callEnded: true,
      };
    case "help":
    case "second-caller":
    case "aokie":
    default:
      return {
        scenario,
        serviceMode: "aokie_active",
        viewerMode: "captions",
        talkOwner: { kind: "aokie" },
        pendingStep: 0,
        recoveryCountdown: 3,
        callEnded: false,
      };
  }
}

export function createInitialState(): PrototypeState {
  const screen: Screen = "home";
  const scenario: Scenario = "aokie";
  const staticMode = false;
  const onboardingStep = 0;

  return {
    screen,
    ...stateForScenario(scenario),
    availability: "available",
    helpOpen: false,
    helpAnswer: "Yes—offer the secure key drop and an inspection tomorrow morning.",
    helpSent: false,
    takeoverOpen: false,
    takeoverConfirmed: false,
    micMuted: false,
    audioRoute: "speaker",
    moreOpen: false,
    endCallOpen: false,
    historyFilter: "All",
    selectedCallId: "CALL-2026-0715-0042",
    onboardingStep,
    deploymentMode: "hosted",
    disclosureEnabled: true,
    staticMode,
    toast: null,
  };
}

export function prototypeReducer(state: PrototypeState, action: PrototypeAction): PrototypeState {
  switch (action.type) {
    case "NAVIGATE":
      return { ...state, screen: action.screen, moreOpen: false };
    case "SET_SCENARIO": {
      const next = stateForScenario(action.scenario);
      return {
        ...state,
        ...next,
        screen: action.scenario === "help" ? "live" : state.screen,
        helpOpen: action.scenario === "help",
        helpSent: false,
        takeoverOpen: false,
        takeoverConfirmed: false,
        endCallOpen: false,
        moreOpen: false,
        micMuted: false,
        toast: null,
      };
    }
    case "SET_AVAILABILITY":
      return { ...state, availability: action.availability, toast: `Availability set to ${action.availability}.` };
    case "OPEN_HELP":
      return { ...state, helpOpen: true, helpSent: false };
    case "CLOSE_HELP":
      return { ...state, helpOpen: false };
    case "SET_HELP_ANSWER":
      return { ...state, helpAnswer: action.value };
    case "SEND_HELP":
      return {
        ...state,
        helpSent: true,
        toast: "Answer delivered privately to Aokie.",
      };
    case "OPEN_TAKEOVER":
      return { ...state, takeoverOpen: true, takeoverConfirmed: false };
    case "CLOSE_TAKEOVER":
      return { ...state, takeoverOpen: false, takeoverConfirmed: false };
    case "ARM_TAKEOVER":
      return { ...state, takeoverConfirmed: true };
    case "START_TAKEOVER":
      return {
        ...state,
        takeoverOpen: false,
        takeoverConfirmed: false,
        scenario: "pending",
        serviceMode: "human_pending",
        viewerMode: "captions",
        talkOwner: { kind: "hold" },
        pendingStep: 1,
        toast: "Takeover claimed. Your microphone is still blocked.",
      };
    case "ADVANCE_PENDING":
      if (state.serviceMode !== "human_pending") return state;
      if (state.pendingStep >= 3) {
        return {
          ...state,
          scenario: "human",
          serviceMode: "human_active",
          viewerMode: "talker",
          talkOwner: { kind: "user", userId: "lance", deviceId: "iphone-15" },
          pendingStep: 4,
          toast: "You are live to the caller.",
        };
      }
      return { ...state, pendingStep: state.pendingStep + 1 };
    case "RETURN_TO_AOKIE":
      return {
        ...state,
        scenario: "aokie",
        serviceMode: "returning_to_aokie",
        viewerMode: "captions",
        talkOwner: { kind: "hold" },
        moreOpen: false,
        toast: "Your microphone is off. Returning to Aokie…",
      };
    case "FINISH_RETURN":
      return {
        ...state,
        scenario: "aokie",
        serviceMode: "aokie_active",
        viewerMode: "captions",
        talkOwner: { kind: "aokie" },
        micMuted: false,
        toast: "Aokie is handling the caller again.",
      };
    case "RECOVERY_TICK":
      return { ...state, recoveryCountdown: Math.max(0, state.recoveryCountdown - 1) };
    case "RECOVERY_FALLBACK":
      return {
        ...state,
        scenario: "aokie",
        serviceMode: "aokie_active",
        viewerMode: "captions",
        talkOwner: { kind: "aokie" },
        recoveryCountdown: 3,
        toast: "Secure recovery complete. Aokie has the caller.",
      };
    case "TOGGLE_MUTE":
      return { ...state, micMuted: !state.micMuted, toast: state.micMuted ? "Microphone live to caller." : "Microphone muted." };
    case "SET_AUDIO_ROUTE":
      return { ...state, audioRoute: action.route, toast: `Audio route: ${action.route}.` };
    case "TOGGLE_MORE":
      return { ...state, moreOpen: !state.moreOpen };
    case "OPEN_END_CALL":
      return { ...state, endCallOpen: true, moreOpen: false };
    case "CLOSE_END_CALL":
      return { ...state, endCallOpen: false };
    case "END_CALL":
      return {
        ...state,
        ...stateForScenario("ended"),
        endCallOpen: false,
        toast: "Caller call ended. Follow-up record created.",
      };
    case "SET_HISTORY_FILTER":
      return { ...state, historyFilter: action.filter };
    case "OPEN_CALL_DETAIL":
      return { ...state, selectedCallId: action.callId, screen: "call-detail" };
    case "SET_ONBOARDING_STEP":
      return { ...state, onboardingStep: action.step };
    case "SET_DEPLOYMENT":
      return { ...state, deploymentMode: action.mode };
    case "SET_STATIC_MODE":
      return { ...state, staticMode: action.value };
    case "TOGGLE_DISCLOSURE":
      return { ...state, disclosureEnabled: !state.disclosureEnabled };
    case "CLEAR_TOAST":
      return { ...state, toast: null };
    case "RESET":
      return createInitialState();
    default:
      return state;
  }
}
