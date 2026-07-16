import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import type { ComponentProps } from "react";
import type {
  CompanionCallRecord,
  CompanionRoutingGroup,
  RuntimeCapabilities,
  V2AssistanceRequestEvent,
  V2CallSnapshotEvent,
} from "../../bridge";
import V2WorkspaceShell, {
  AudioEndpointSettings,
  CallDetailScreen,
  CompanionActivityList,
  HistoryScreen,
  ServerProfileSettings,
  SettingsScreen,
  activeTrustedServerProfile,
  availabilityExpiryLabel,
  boundedFingerprint,
  callRecordFilters,
  callRecordTone,
  callRecordsHaveMore,
  connectedParticipantCounts,
  connectedParticipantSummary,
  countAvailableRoutingMembers,
  exactCallRecordId,
  findCurrentStaff,
  filterCallRecords,
  loadedCallRecordCountLabel,
  nextCallRecordLimit,
  settingsValueTone,
  sortRoutingMembers,
  staffCountLabel,
  staffGreeting,
  visibleCallRecordWindow,
} from "./V2WorkspaceShell";

const runtime: RuntimeCapabilities = {
  platform: "windows",
  tauri: true,
  secureStorage: true,
  notifications: true,
  microphone: true,
  nativeCallUi: false,
  realtime: true,
  mediaBridge: true,
  demo: false,
  localPilot: true,
  notificationPermission: "granted",
  microphonePermission: "prompt",
  fcmConfigured: false,
  fcmTokenPresent: false,
  pushRegistration: "not_applicable",
  pendingCallOffer: false,
  batteryOptimizationsRestricted: false,
  forceStopState: "not_detectable",
  callInfrastructure: "foreground",
  lastNativeDiagnostic: null,
};

const snapshot: V2CallSnapshotEvent = {
  kind: "snapshot",
  schemaVersion: 2,
  appId: "app_test",
  sequence: 4,
  grants: ["state_read", "caller_read"],
  snapshot: {
    callId: "call_authoritative",
    callEpoch: 2,
    ownerEpoch: 3,
    switchboardRevision: 4,
    remoteRevision: 5,
    telephonyState: "active",
    serviceMode: "aokie_active",
    mediaState: "ready",
    remoteCapabilities: {
      softwareHold: true,
      carrierHoldEvidence: "proven",
      secondaryCallObservation: "observed",
      voiceConsult: true,
      takeover: true,
    },
    secondaryCallPolicy: "miss_and_callback",
    remoteConsent: {
      policyId: "policy_test",
      policyVersion: 1,
      enabled: true,
      acknowledged: true,
      acknowledgedAt: "2026-07-16T00:00:00Z",
      captionsEnabled: false,
      assistanceEnabled: true,
      monitorEnabled: false,
      consultEnabled: false,
      takeoverEnabled: false,
    },
    caller: { label: "Authoritative caller", maskedNumber: "••• 431" },
    captions: [],
    participants: [],
    pendingMobileOffers: [],
    occurredAt: "2026-07-16T00:00:00Z",
  },
};

const assistance: V2AssistanceRequestEvent = {
  kind: "assistance_request",
  schemaVersion: 2,
  appId: "app_test",
  eventId: "event_help_1",
  requestId: "request_help_1",
  callId: snapshot.snapshot.callId,
  callEpoch: snapshot.snapshot.callEpoch,
  ownerEpoch: snapshot.snapshot.ownerEpoch,
  switchboardRevision: snapshot.snapshot.switchboardRevision,
  remoteRevision: snapshot.snapshot.remoteRevision,
  question: "Can the caller use the after-hours key drop?",
  context: "Existing customer",
  expiresAt: 2_000_000_000,
};

type WorkspaceProps = ComponentProps<typeof V2WorkspaceShell>;

const baseProps: WorkspaceProps = {
  runtime,
  transport: "connected",
  snapshot: null,
  idleSync: null,
  assistance: null,
  loadBootstrap: async () => { throw new Error("not invoked during server render"); },
  loadHistory: async () => ({ activity: [], sessions: [] }),
  loadRouting: async () => ({ routingGroups: [], staff: [] }),
  loadCallRecords: async () => ({ records: [], access: "full" }),
  loadCallRecordDetail: async () => { throw new Error("not invoked during server render"); },
  loadAudioDevices: async () => ({ routingPolicy: "unavailable", inputDevices: [], outputDevices: [], selectedInputId: "", selectedOutputId: "", state: "idle", canSelect: false }),
  loadServerProfiles: async () => [],
  selectAudioDevices: async () => { throw new Error("not selectable"); },
  setAvailability: async () => null,
  renderLiveCall: () => <div>authoritative live call</div>,
  onDisconnect: async () => undefined,
  onForgetManaged: async () => undefined,
};

function renderWorkspace(overrides: Partial<WorkspaceProps> = {}): string {
  return renderToStaticMarkup(<V2WorkspaceShell {...baseProps} {...overrides} />);
}

describe("server-backed Companion workspace", () => {
  it("keeps the authoritative active-call capsule available above the tab bar", () => {
    const html = renderWorkspace({ snapshot });

    expect(html).toContain("Active Companion work");
    expect(html).toContain("live-authoritative-capsule");
    expect(html).toContain("Authoritative caller");
    expect(html).toContain("Desktop state and server signalling are current");
    expect(html).toContain("mic/speaker endpoint");
    expect(html).not.toContain("Mia");
    expect(html).not.toContain("Coastal Auto Care");
    expect(html).not.toContain("Lance");
  });

  it("persists an assistance card with the server-provided question", () => {
    const html = renderWorkspace({ snapshot, assistance });

    expect(html).toContain("live-persistent-help");
    expect(html).toContain("Aokie needs your answer");
    expect(html).toContain("Can the caller use the after-hours key drop?");
  });

  it("does not claim that Desktop is current from the mobile socket alone", () => {
    const html = renderWorkspace({ transport: "connected", snapshot: null, idleSync: null });

    expect(html).toContain("Server connected — waiting for Desktop state");
    expect(html).not.toContain("Desktop state and server signalling are current");
    expect(html).not.toContain("live-authoritative-capsule");
  });

  it("shows an authenticated no-call connection without active-work controls", () => {
    const html = renderWorkspace({
      idleSync: { kind: "idle_sync", schemaVersion: 2, appId: "app_test", sequence: 6, grants: ["state_read"] },
    });

    expect(html).toContain("Connected — waiting for a live Aokie call");
    expect(html).not.toContain("Server connected — waiting for Desktop state");
    expect(html).not.toContain("authoritative live call");
    expect(html).not.toContain("Active Companion work");
  });

  it("counts only current connected or active participants and identifies observers", () => {
    const liveSnapshot: V2CallSnapshotEvent = {
      ...snapshot,
      snapshot: {
        ...snapshot.snapshot,
        participants: [
          { participantId: "observer_connected", mode: "observer", state: "connected", displayLabel: "Observer" },
          { participantId: "talker_active", mode: "talker", state: "active", displayLabel: "Talker" },
          { participantId: "advisor_prepared", mode: "advisor", state: "prepared", displayLabel: "Prepared advisor" },
        ],
      },
    };

    const html = renderWorkspace({ snapshot: liveSnapshot });

    expect(html).toContain("2 connected participants · 1 observer");
    expect(html).not.toContain("3 connected participants");
    expect(connectedParticipantCounts([
      { mode: "observer", state: "connected" },
      { mode: "talker", state: "active" },
      { mode: "advisor", state: "prepared" },
      { mode: "observer", state: "disconnected" },
    ])).toEqual({ connected: 2, observers: 1 });
    expect(connectedParticipantSummary([{ mode: "observer", state: "connected" }])).toBe("1 connected participant · 1 observer");
  });
});

const callRecords: CompanionCallRecord[] = [
  {
    id: "record_complete",
    callId: "call_complete",
    callerName: "Visible customer",
    maskedNumber: "••• 100",
    status: "completed",
    direction: "inbound",
    summary: "Completed booking",
    startedAt: "2026-07-16T00:00:00Z",
    endedAt: "2026-07-16T00:03:00Z",
    durationSeconds: 180,
    followUpRequired: false,
    submittedAt: "2026-07-16T00:03:00Z",
  },
  {
    id: "record_missed",
    callId: "call_missed",
    callerName: null,
    maskedNumber: null,
    status: "missed",
    direction: "inbound",
    summary: null,
    startedAt: null,
    endedAt: null,
    durationSeconds: null,
    followUpRequired: true,
    submittedAt: "2026-07-16T01:00:00Z",
  },
];

describe("FormLogic call-record presentation helpers", () => {
  it("advances bounded call-record windows and labels partial results", () => {
    expect(nextCallRecordLimit(0)).toBe(25);
    expect(nextCallRecordLimit(25)).toBe(50);
    expect(nextCallRecordLimit(90)).toBe(100);
    expect(nextCallRecordLimit(Number.NaN)).toBe(25);
    expect(callRecordsHaveMore(25, 25, "full")).toBe(true);
    expect(callRecordsHaveMore(24, 25, "full")).toBe(false);
    expect(callRecordsHaveMore(26, 25, "full")).toBe(false);
    expect(callRecordsHaveMore(25, 25, "none")).toBe(false);
    expect(callRecordsHaveMore(100, 100, "full")).toBe(false);
    expect(loadedCallRecordCountLabel(25, true)).toBe("25 shown");
    expect(loadedCallRecordCountLabel(24, false)).toBe("24 records");
    const manyRecords = Array.from({ length: 30 }, (_, index) => ({ ...callRecords[0], id: `record_${index}` }));
    expect(visibleCallRecordWindow(manyRecords, 25)).toHaveLength(25);
    expect(visibleCallRecordWindow(manyRecords, 50)).toHaveLength(30);
    expect(visibleCallRecordWindow(manyRecords, Number.NaN)).toHaveLength(0);
  });

  it("renders the accessible progressive-loading fallback for partial records", () => {
    const html = renderToStaticMarkup(<HistoryScreen
      records={{ records: callRecords, access: "full" }}
      error={null}
      hasMore
      onRefresh={() => undefined}
      onLoadMore={() => undefined}
      onOpenRecord={() => undefined}
    />);

    expect(html).toContain("2 shown");
    expect(html).toContain("Load older calls");
    expect(html).toContain("More records load automatically as you scroll");
    expect(html).toContain('aria-pressed="true"');
    expect(html).not.toContain("All available records loaded");
  });

  it("filters only real server records without manufacturing outcomes", () => {
    expect(filterCallRecords(callRecords, "all")).toHaveLength(2);
    expect(filterCallRecords(callRecords, "status:completed").map((record) => record.id)).toEqual(["record_complete"]);
    expect(filterCallRecords(callRecords, "status:missed").map((record) => record.id)).toEqual(["record_missed"]);
    expect(filterCallRecords(callRecords, "follow_up").map((record) => record.id)).toEqual(["record_missed"]);
    expect(callRecordFilters(callRecords).map((option) => option.value)).toEqual(["all", "follow_up", "status:completed", "status:missed"]);
  });

  it("derives semantic tone from status and follow-up facts", () => {
    expect(callRecordTone(callRecords[0])).toBe("handled");
    expect(callRecordTone(callRecords[1])).toBe("danger");
  });

  it("links an ended live view only when one loaded record exactly matches its call", () => {
    expect(exactCallRecordId({ records: callRecords, access: "full" }, "call_complete")).toBe("record_complete");
    expect(exactCallRecordId({ records: callRecords, access: "full" }, "call_unknown")).toBeNull();
    expect(exactCallRecordId({ records: callRecords, access: "none" }, "call_complete")).toBeNull();
    expect(exactCallRecordId({ records: [callRecords[0], { ...callRecords[0], id: "record_duplicate" }], access: "full" }, "call_complete")).toBeNull();
  });

  it("renders a truthful role or pack empty state", () => {
    const html = renderToStaticMarkup(<HistoryScreen records={{ records: [], access: "none" }} error={null} onRefresh={() => undefined} onOpenRecord={() => undefined} />);

    expect(html).toContain("Call records are not available to this role");
    expect(html).toContain("did not grant call-record access");
    expect(html).not.toContain("Calls today");
    expect(html).not.toContain("<strong>18</strong>");
  });

  it("labels preserved records as stale when a refresh fails", () => {
    const html = renderToStaticMarkup(<HistoryScreen
      records={{ records: callRecords, access: "full" }}
      error="The server could not be reached"
      lastUpdatedAt={Date.UTC(2026, 6, 16, 1, 2)}
      onRefresh={() => undefined}
      onOpenRecord={() => undefined}
    />);

    expect(html).toContain("Showing the last successful FormLogic response");
    expect(html).toContain("The server could not be reached");
    expect(html).toContain("Visible customer");
  });

  it("renders only the record detail returned by FormLogic", () => {
    const html = renderToStaticMarkup(<CallDetailScreen
      detail={{
        record: callRecords[0],
        transcript: [{ id: "turn_1", speaker: "Caller", text: "Server-provided transcript", occurredAt: "2026-07-16T00:01:00Z" }],
        followUps: [{ id: "follow_1", summary: "Server-provided follow-up", status: "open", priority: "high", submittedAt: "2026-07-16T00:03:00Z" }],
      }}
      activity={[{
        id: "activity_1",
        eventId: "event_1",
        appId: "app_test",
        sessionRecordId: "session_1",
        callId: "call_complete",
        deviceId: null,
        actorUserId: null,
        subjectId: "subject_test",
        eventType: "returned_to_aokie",
        mode: "takeover",
        reason: "operator_return",
        ownerEpoch: 4,
        occurredAt: "2026-07-16T00:02:00Z",
      }]}
      loading={false}
      error={null}
      onBack={() => undefined}
    />);

    expect(html).toContain("Completed booking");
    expect(html).toContain("Server-provided transcript");
    expect(html).toContain("Server-provided follow-up");
    expect(html).toContain("Companion activity");
    expect(html).toContain("Returned To Aokie");
    expect(html).not.toContain("Mia");
  });

  it("renders general role-filtered activity without raw actor or subject identifiers", () => {
    const html = renderToStaticMarkup(<CompanionActivityList activity={[{
      id: "activity_internal_key",
      eventId: "event_internal_key",
      appId: "app_internal_key",
      sessionRecordId: "session_internal_key",
      callId: "call_internal_key",
      deviceId: "device_secret_raw",
      actorUserId: "actor_secret_raw",
      subjectId: "subject_secret_raw",
      eventType: "consult_joined",
      mode: "consult",
      reason: "subject_secret_raw",
      ownerEpoch: 4,
      occurredAt: "2026-07-16T00:02:00Z",
    }]} />);

    expect(html).toContain("Consult Joined");
    expect(html).toContain("Consult");
    expect(html).not.toContain("actor_secret_raw");
    expect(html).not.toContain("subject_secret_raw");
    expect(html).not.toContain("device_secret_raw");
    expect(html).not.toContain("call_internal_key");
  });
});

const routingGroups: CompanionRoutingGroup[] = [{
  id: "routing_primary",
  name: "Primary",
  policy: "priority",
  enabled: true,
  members: [
    { staffId: "staff_b", displayName: "Second responder", roleName: "Advisor", isCurrentUser: false, priority: 2, enabled: true, availability: "available", availabilityUpdatedAt: "2026-07-16T00:00:00Z", availabilityExpiresAt: null },
    { staffId: "staff_a", displayName: "First responder", roleName: "Owner", isCurrentUser: true, priority: 1, enabled: true, availability: "available", availabilityUpdatedAt: "2026-07-16T00:00:00Z", availabilityExpiresAt: null },
  ],
}, {
  id: "routing_duplicate",
  name: "Escalation",
  policy: "all",
  enabled: true,
  members: [
    { staffId: "staff_a", displayName: "First responder", roleName: "Owner", isCurrentUser: true, priority: 1, enabled: true, availability: "available", availabilityUpdatedAt: "2026-07-16T00:00:00Z", availabilityExpiresAt: null },
  ],
}];

describe("enriched read-only routing helpers", () => {
  it("orders members by server priority without mutating the response", () => {
    const original = routingGroups[0].members.map((member) => member.staffId);
    expect(sortRoutingMembers(routingGroups[0].members).map((member) => member.staffId)).toEqual(["staff_a", "staff_b"]);
    expect(routingGroups[0].members.map((member) => member.staffId)).toEqual(original);
  });

  it("deduplicates available staff across routing groups", () => {
    expect(countAvailableRoutingMembers(routingGroups)).toBe(2);
  });

  it("does not claim coverage from a disabled routing group", () => {
    expect(countAvailableRoutingMembers([{ ...routingGroups[0], enabled: false }])).toBe(0);
  });

  it("selects the actual current FormLogic staff identity", () => {
    const staff = [
      { id: "staff_other", displayName: "Other staff", roleName: "Advisor", isCurrentUser: false, isOwner: false, companionReady: true },
      { id: "staff_current", displayName: "Current staff", roleName: "Owner", isCurrentUser: true, isOwner: true, companionReady: true },
    ];
    expect(findCurrentStaff(staff)?.id).toBe("staff_current");
    expect(staffGreeting(staff, 9)).toBe("Good morning, Current");
    expect(staffGreeting(staff, 19)).toBe("Good evening, Current");
    expect(findCurrentStaff([])).toBeNull();
    expect(staffGreeting([], 9)).toBe("Aokie Companion");
  });

  it("uses correct singular and plural staff labels", () => {
    expect(staffCountLabel(0)).toBe("0 people");
    expect(staffCountLabel(1)).toBe("1 person");
    expect(staffCountLabel(2)).toBe("2 people");
  });
});

describe("truthful settings facts", () => {
  it("does not decorate denied or unknown permissions as successful", () => {
    expect(settingsValueTone("granted")).toBe("ready");
    expect(settingsValueTone("denied")).toBe("warning");
    expect(settingsValueTone("configuration_required")).toBe("warning");
    expect(settingsValueTone("prompt")).toBe("neutral");
    expect(settingsValueTone("0")).toBe("neutral");
  });

  it("shows exact server availability expiry instead of a perpetual local status", () => {
    const now = Date.parse("2026-07-16T00:00:00Z");
    expect(availabilityExpiryLabel({
      availability: "available",
      updatedAt: "2026-07-16T00:00:00Z",
      expiresAt: "2026-07-16T08:00:00Z",
    }, now)).toContain("Available until");
    expect(availabilityExpiryLabel({
      availability: "do_not_disturb",
      updatedAt: "2026-07-15T23:00:00Z",
      expiresAt: "2026-07-16T00:00:00Z",
    }, now)).toContain("expired");
    expect(availabilityExpiryLabel(null, now)).toContain("server-enforced");
  });

  it("shows only the active trusted server with bounded fingerprints", () => {
    const profile = {
      profileId: "profile_secret_raw",
      serverUrl: "https://companion.example.test/api/aokie-companion",
      origin: "https://companion.example.test",
      deploymentId: "deployment_public",
      appId: "app_secret_raw",
      deviceId: "device_secret_raw",
      discoveryFingerprint: "discovery-fingerprint-abcdefghijklmnopqrstuvwxyz-0123456789",
      endpointFingerprint: "endpoint-fingerprint-abcdefghijklmnopqrstuvwxyz-0123456789",
      trustState: "trusted" as const,
      active: true,
    };
    const html = renderToStaticMarkup(<ServerProfileSettings profiles={[profile]} error={null} />);

    expect(activeTrustedServerProfile([profile])).toEqual(profile);
    expect(activeTrustedServerProfile([profile, { ...profile, profileId: "profile_duplicate" }])).toBeNull();
    expect(html).toContain("https://companion.example.test");
    expect(html).toContain("deployment_public");
    expect(html).toContain(boundedFingerprint(profile.discoveryFingerprint));
    expect(html).toContain(boundedFingerprint(profile.endpointFingerprint));
    expect(html).not.toContain(profile.discoveryFingerprint);
    expect(html).not.toContain(profile.endpointFingerprint);
    expect(html).not.toContain("profile_secret_raw");
    expect(html).not.toContain("app_secret_raw");
    expect(html).not.toContain("device_secret_raw");
    expect(boundedFingerprint(profile.discoveryFingerprint)).toHaveLength(24);
  });

  it("keeps selectable audio endpoints locked unless native state and canSelect both permit changes", () => {
    const devices = {
      routingPolicy: "selectable" as const,
      inputDevices: [{ id: "input_1", label: "Test microphone" }],
      outputDevices: [{ id: "output_1", label: "Test speakers" }],
      selectedInputId: "input_1",
      selectedOutputId: "output_1",
      state: "media_active" as const,
      canSelect: true,
    };
    const locked = renderToStaticMarkup(<AudioEndpointSettings devices={devices} busy={false} onSelection={async () => undefined} />);
    const idle = renderToStaticMarkup(<AudioEndpointSettings devices={{ ...devices, state: "idle" }} busy={false} onSelection={async () => undefined} />);
    const denied = renderToStaticMarkup(<AudioEndpointSettings devices={{ ...devices, state: "idle", canSelect: false }} busy={false} onSelection={async () => undefined} />);

    expect(locked.match(/disabled=""/g)).toHaveLength(2);
    expect(denied.match(/disabled=""/g)).toHaveLength(2);
    expect(idle).not.toContain("disabled");
    expect(locked).toContain("Device changes are locked while call media is active");
  });

  it("surfaces native background limitations and diagnostics from the runtime", () => {
    const html = renderToStaticMarkup(<SettingsScreen
      bootstrap={null}
      staff={[]}
      runtime={{
        ...runtime,
        batteryOptimizationsRestricted: true,
        pendingCallOffer: true,
        forceStopState: "not_detectable",
        lastNativeDiagnostic: "Background call alert was delayed by the operating system",
      }}
      transport="connected"
      audioDevices={null}
      audioBusy={false}
      audioError={null}
      serverProfiles={null}
      serverProfilesError={null}
      onAudioSelection={async () => undefined}
      onRefresh={() => undefined}
      onDisconnect={async () => undefined}
      onForgetManaged={async () => undefined}
      onRunSetup={() => undefined}
    />);

    expect(html).toContain("Battery optimisation");
    expect(html).toContain("Pending native offer");
    expect(html).toContain("Force-stop visibility");
    expect(html).toContain("Encrypted media boundary");
    expect(html).toContain("Background call alert was delayed by the operating system");
  });
});
