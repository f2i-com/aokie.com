import {
  AlertTriangle,
  ArrowLeft,
  Bell,
  Bot,
  CheckCircle2,
  ChevronRight,
  Clock3,
  Headphones,
  History,
  Home,
  LockKeyhole,
  MessageSquare,
  Mic,
  PhoneCall,
  RefreshCw,
  Server,
  Settings,
  ShieldCheck,
  Sparkles,
  Smartphone,
  Users,
  Wifi,
  WifiOff,
  type LucideIcon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type {
  CompanionActivity,
  CompanionAvailabilityRecord,
  CompanionAvailabilityState,
  CompanionBootstrap,
  CompanionCallRecord,
  CompanionCallRecordDetail,
  CompanionCallRecords,
  CompanionHistory,
  CompanionRouting,
  CompanionRoutingGroup,
  CompanionSession,
  CompanionStaffMember,
  NativeAudioDevices,
  RuntimeCapabilities,
  ServerProfile,
  V2AssistanceRequestEvent,
  V2CallSnapshotEvent,
  V2IdleSyncEvent,
} from "../../bridge";
import { displayError } from "../../utils/displayError";

export type CompanionAvailabilityValue = CompanionAvailabilityState;
export type CompanionAvailabilityView = CompanionAvailabilityRecord;
export type CompanionActivityView = CompanionActivity;
export type CompanionSessionView = CompanionSession;
export type CompanionHistoryView = CompanionHistory;
export type CompanionRoutingGroupView = CompanionRoutingGroup;
export type CompanionBootstrapView = CompanionBootstrap;

type WorkspaceScreen = "home" | "live" | "calls" | "call-detail" | "team" | "settings";
type Transport = "idle" | "connecting" | "connected" | "reconnecting" | "offline";
export type CallRecordFilter = "all" | "follow_up" | `status:${string}`;

export const CALL_RECORD_PAGE_SIZE = 25;
export const CALL_RECORD_MAX_LIMIT = 100;

export function nextCallRecordLimit(currentLimit: number): number {
  const current = Number.isFinite(currentLimit) ? Math.max(0, Math.floor(currentLimit)) : 0;
  return Math.min(CALL_RECORD_MAX_LIMIT, Math.max(CALL_RECORD_PAGE_SIZE, current + CALL_RECORD_PAGE_SIZE));
}

export function callRecordsHaveMore(recordCount: number, requestedLimit: number, access: CompanionCallRecords["access"]): boolean {
  return access !== "none" && requestedLimit < CALL_RECORD_MAX_LIMIT && recordCount === requestedLimit;
}

export function loadedCallRecordCountLabel(count: number, hasMore: boolean): string {
  return `${Math.max(0, count)} ${hasMore ? "shown" : "records"}`;
}

export function visibleCallRecordWindow(records: CompanionCallRecord[], visibleLimit: number): CompanionCallRecord[] {
  const limit = Number.isFinite(visibleLimit) ? Math.max(0, Math.floor(visibleLimit)) : 0;
  return records.slice(0, Math.min(CALL_RECORD_MAX_LIMIT, limit));
}

interface V2WorkspaceShellProps {
  runtime: RuntimeCapabilities;
  transport: Transport;
  snapshot: V2CallSnapshotEvent | null;
  idleSync: V2IdleSyncEvent | null;
  assistance: V2AssistanceRequestEvent | null;
  loadBootstrap(): Promise<CompanionBootstrapView>;
  loadHistory(limit?: number, before?: number): Promise<CompanionHistoryView>;
  loadRouting(): Promise<CompanionRouting>;
  loadCallRecords(limit?: number): Promise<CompanionCallRecords>;
  loadCallRecordDetail(recordId: string): Promise<CompanionCallRecordDetail>;
  loadAudioDevices(): Promise<NativeAudioDevices>;
  loadServerProfiles(): Promise<ServerProfile[]>;
  selectAudioDevices(inputId: string, outputId: string): Promise<NativeAudioDevices>;
  setAvailability(value: CompanionAvailabilityValue, expiresInSeconds?: number): Promise<CompanionAvailabilityView | null>;
  renderLiveCall(
    onHome: () => void,
    audioEndpointControls: ReactNode,
    onCalls: () => void,
    callRecordId: string | null,
  ): ReactNode;
  onDisconnect(): Promise<void>;
  onForgetManaged(): Promise<void>;
  onRunSetup?(): void | Promise<void>;
}

const NAV_ITEMS: Array<{ id: Exclude<WorkspaceScreen, "live" | "call-detail">; label: string; icon: LucideIcon }> = [
  { id: "home", label: "Home", icon: Home },
  { id: "calls", label: "Calls", icon: PhoneCall },
  { id: "team", label: "Team", icon: Users },
  { id: "settings", label: "Settings", icon: Settings },
];

const AVAILABILITY_OPTIONS: Array<{ value: CompanionAvailabilityValue; label: string }> = [
  { value: "available", label: "Available" },
  { value: "do_not_disturb", label: "Quiet" },
  { value: "offline", label: "Unavailable" },
];

export default function V2WorkspaceShell({
  runtime,
  transport,
  snapshot,
  idleSync,
  assistance,
  loadBootstrap,
  loadHistory,
  loadRouting,
  loadCallRecords,
  loadCallRecordDetail,
  loadAudioDevices,
  loadServerProfiles,
  selectAudioDevices,
  setAvailability,
  renderLiveCall,
  onDisconnect,
  onForgetManaged,
  onRunSetup,
}: V2WorkspaceShellProps) {
  const [screen, setScreen] = useState<WorkspaceScreen>("home");
  const [bootstrap, setBootstrap] = useState<CompanionBootstrapView | null>(null);
  const [routingGroups, setRoutingGroups] = useState<CompanionRoutingGroupView[]>([]);
  const [staff, setStaff] = useState<CompanionStaffMember[]>([]);
  const [callRecords, setCallRecords] = useState<CompanionCallRecords | null>(null);
  const [callRecordsError, setCallRecordsError] = useState<string | null>(null);
  const [callRecordsUpdatedAt, setCallRecordsUpdatedAt] = useState<number | null>(null);
  const [callRecordsHasMore, setCallRecordsHasMore] = useState(false);
  const [callRecordsLoadingMore, setCallRecordsLoadingMore] = useState(false);
  const callRecordsLimit = useRef(CALL_RECORD_PAGE_SIZE);
  const callRecordsRequest = useRef(0);
  const [callDetail, setCallDetail] = useState<CompanionCallRecordDetail | null>(null);
  const [callDetailLoading, setCallDetailLoading] = useState(false);
  const [callDetailError, setCallDetailError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [availabilityBusy, setAvailabilityBusy] = useState(false);
  const [audioDevices, setAudioDevices] = useState<NativeAudioDevices | null>(null);
  const [audioBusy, setAudioBusy] = useState(false);
  const [audioError, setAudioError] = useState<string | null>(null);
  const [serverProfiles, setServerProfiles] = useState<ServerProfile[] | null>(null);
  const [serverProfilesError, setServerProfilesError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refreshCallRecords = useCallback(async (requestedLimit = callRecordsLimit.current, loadingMore = false) => {
    const limit = Math.min(CALL_RECORD_MAX_LIMIT, Math.max(1, Math.floor(requestedLimit)));
    callRecordsLimit.current = Math.max(callRecordsLimit.current, limit);
    const request = ++callRecordsRequest.current;
    setCallRecordsLoadingMore(loadingMore);
    try {
      const records = await loadCallRecords(limit);
      if (request !== callRecordsRequest.current) return;
      setCallRecords(records);
      setCallRecordsHasMore(callRecordsHaveMore(records.records.length, limit, records.access));
      setCallRecordsError(null);
      setCallRecordsUpdatedAt(Date.now());
    } catch (caught) {
      if (request !== callRecordsRequest.current) return;
      setCallRecordsError(displayError(caught, "FormLogic call records are unavailable"));
    } finally {
      if (request === callRecordsRequest.current) setCallRecordsLoadingMore(false);
    }
  }, [loadCallRecords]);

  const loadMoreCallRecords = useCallback(() => {
    if (callRecordsLoadingMore || !callRecordsHasMore) return;
    void refreshCallRecords(nextCallRecordLimit(callRecordsLimit.current), true);
  }, [callRecordsHasMore, callRecordsLoadingMore, refreshCallRecords]);

  const refresh = useCallback(async (quiet = false) => {
    if (quiet) setRefreshing(true);
    else setLoading(true);
    try {
      const value = await loadBootstrap();
      setBootstrap(value);
      setRoutingGroups(value.routingGroups);
      setStaff(value.staff);
      setError(null);
      void refreshCallRecords();
    } catch (caught) {
      setError(displayError(caught, "Companion workspace could not be refreshed"));
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  }, [loadBootstrap, refreshCallRecords]);

  useEffect(() => { void refresh(); }, [refresh]);
  useEffect(() => {
    if (transport !== "connected") return;
    const timer = window.setInterval(() => void refresh(true), 30_000);
    return () => window.clearInterval(timer);
  }, [refresh, transport]);

  const refreshAudioDevices = useCallback(async () => {
    try {
      setAudioDevices(await loadAudioDevices());
      setAudioError(null);
      setError(null);
    } catch (caught) {
      const message = displayError(caught, "Audio endpoints could not be enumerated");
      setAudioError(message);
      setError(message);
    }
  }, [loadAudioDevices]);

  const refreshServerProfiles = useCallback(async () => {
    try {
      setServerProfiles(await loadServerProfiles());
      setServerProfilesError(null);
    } catch (caught) {
      setServerProfilesError(displayError(caught, "Trusted server details could not be loaded"));
    }
  }, [loadServerProfiles]);

  const navigate = useCallback(async (next: WorkspaceScreen) => {
    setScreen(next);
    if (next === "calls") {
      await refreshCallRecords();
    }
    if (next === "team") {
      try {
        const routing = await loadRouting();
        setRoutingGroups(routing.routingGroups);
        setStaff(routing.staff);
        setError(null);
      } catch (caught) {
        setError(displayError(caught, "Team routing could not be refreshed"));
      }
    }
    if (next === "settings") {
      await Promise.all([refreshAudioDevices(), refreshServerProfiles()]);
    }
  }, [loadRouting, refreshAudioDevices, refreshCallRecords, refreshServerProfiles]);

  useEffect(() => {
    if (screen !== "live") return;
    void refreshAudioDevices();
  }, [refreshAudioDevices, screen]);

  const openCallRecord = useCallback(async (recordId: string) => {
    setScreen("call-detail");
    setCallDetail(null);
    setCallDetailError(null);
    setCallDetailLoading(true);
    void loadHistory(100).then((history) => {
      setBootstrap((current) => current ? { ...current, history } : current);
    }).catch(() => undefined);
    try {
      setCallDetail(await loadCallRecordDetail(recordId));
    } catch (caught) {
      setCallDetailError(displayError(caught, "This FormLogic call record could not be opened"));
    } finally {
      setCallDetailLoading(false);
    }
  }, [loadCallRecordDetail, loadHistory]);

  const updateAudioDevices = useCallback(async (inputId: string, outputId: string) => {
    if (audioBusy) return;
    setAudioBusy(true);
    try {
      setAudioDevices(await selectAudioDevices(inputId, outputId));
      setAudioError(null);
      setError(null);
    } catch (caught) {
      const message = displayError(caught, "The selected audio endpoints could not be applied");
      setAudioError(message);
      setError(message);
      try { setAudioDevices(await loadAudioDevices()); } catch { /* preserve the primary selection error */ }
    } finally {
      setAudioBusy(false);
    }
  }, [audioBusy, loadAudioDevices, selectAudioDevices]);

  const updateAvailability = useCallback(async (value: CompanionAvailabilityValue) => {
    if (!bootstrap || availabilityBusy) return;
    setAvailabilityBusy(true);
    try {
      const availability = await setAvailability(value, value === "available" ? 8 * 60 * 60 : 60 * 60);
      setBootstrap((current) => current ? { ...current, availability } : current);
      setError(null);
    } catch (caught) {
      setError(displayError(caught, "Availability could not be updated"));
    } finally {
      setAvailabilityBusy(false);
    }
  }, [availabilityBusy, bootstrap, setAvailability]);

  const call = snapshot?.snapshot;
  const callRecordId = exactCallRecordId(callRecords, call?.callId);
  if (screen === "live") {
    return <>{renderLiveCall(
      () => setScreen("home"),
      <AudioEndpointSettings devices={audioDevices} busy={audioBusy} error={audioError} onSelection={updateAudioDevices} />,
      () => { if (callRecordId) void openCallRecord(callRecordId); else void navigate("calls"); },
      callRecordId,
    )}</>;
  }

  const activeCall = call && call.telephonyState !== "ended" ? call : null;

  return (
    <main className="live-workspace-root">
      <div className="live-workspace-scroll">
        {screen === "home" && (
          <HomeScreen
            bootstrap={bootstrap}
            snapshot={snapshot}
            idleSync={idleSync}
            transport={transport}
            loading={loading}
            refreshing={refreshing}
            availabilityBusy={availabilityBusy}
            onAvailability={updateAvailability}
            onOpenCall={() => setScreen("live")}
            onRefresh={() => void refresh(true)}
            onOpenHistory={() => void navigate("calls")}
            callRecords={callRecords}
            onOpenRecord={(recordId) => void openCallRecord(recordId)}
          />
        )}
        {screen === "calls" && (
          <HistoryScreen
            records={callRecords}
            error={callRecordsError}
            lastUpdatedAt={callRecordsUpdatedAt}
            hasMore={callRecordsHasMore}
            loadingMore={callRecordsLoadingMore}
            onRefresh={() => void refreshCallRecords()}
            onLoadMore={loadMoreCallRecords}
            onOpenRecord={(recordId) => void openCallRecord(recordId)}
          />
        )}
        {screen === "call-detail" && (
          <CallDetailScreen
            detail={callDetail}
            loading={callDetailLoading}
            error={callDetailError}
            activity={(bootstrap?.history.activity ?? []).filter((item) => item.callId === callDetail?.record.callId)}
            onBack={() => setScreen("calls")}
          />
        )}
        {screen === "team" && <TeamScreen groups={routingGroups} staff={staff} />}
        {screen === "settings" && (
          <SettingsScreen
            bootstrap={bootstrap}
            staff={staff}
            runtime={runtime}
            transport={transport}
            audioDevices={audioDevices}
            audioBusy={audioBusy}
            audioError={audioError}
            serverProfiles={serverProfiles}
            serverProfilesError={serverProfilesError}
            onAudioSelection={updateAudioDevices}
            onRefresh={() => { void refresh(true); void refreshAudioDevices(); void refreshServerProfiles(); }}
            onDisconnect={onDisconnect}
            onForgetManaged={onForgetManaged}
            onRunSetup={onRunSetup}
          />
        )}
        {error && <div className="runtime-error live-workspace-error" role="alert"><WifiOff size={18} /><span>{error}</span></div>}
      </div>
      {(assistance || activeCall) && (
        <div className="live-workspace-persistent" aria-label="Active Companion work">
          {assistance && (
            <button className="home-help-card live-persistent-help" onClick={() => setScreen("live")}>
              <span className="help-orb"><Sparkles size={18} /></span>
              <span><strong>Aokie needs your answer</strong><small>{assistance.question}</small></span>
              <span className="live-help-badge">Help</span>
              <ChevronRight size={18} />
            </button>
          )}
          {activeCall && (
            <button className={`persistent-call live-authoritative-capsule ${transport === "connected" ? "" : "is-unconfirmed"}`} onClick={() => setScreen("live")}>
              <span className="pulse-dot" />
              <span>
                <strong>{activeCall.caller?.label ?? "Caller identity hidden"}</strong>
                <small>{transport === "connected" ? humanize(activeCall.serviceMode) : `Connection ${humanize(transport)} · last state ${humanize(activeCall.serviceMode)}`}</small>
              </span>
              <PhoneCall size={17} />
              <ChevronRight size={17} />
            </button>
          )}
        </div>
      )}
      <nav className="mobile-nav live-workspace-nav" aria-label="Companion navigation">
        {NAV_ITEMS.map(({ id, label, icon: Icon }) => (
          <button key={id} className={screen === id || (screen === "call-detail" && id === "calls") ? "is-active" : ""} onClick={() => void navigate(id)}>
            <Icon size={19} /><span>{label}</span>
          </button>
        ))}
      </nav>
    </main>
  );
}

function HomeScreen({
  bootstrap,
  snapshot,
  idleSync,
  transport,
  loading,
  refreshing,
  availabilityBusy,
  onAvailability,
  onOpenCall,
  onRefresh,
  onOpenHistory,
  callRecords,
  onOpenRecord,
}: {
  bootstrap: CompanionBootstrapView | null;
  snapshot: V2CallSnapshotEvent | null;
  idleSync: V2IdleSyncEvent | null;
  transport: Transport;
  loading: boolean;
  refreshing: boolean;
  availabilityBusy: boolean;
  onAvailability(value: CompanionAvailabilityValue): void;
  onOpenCall(): void;
  onRefresh(): void;
  onOpenHistory(): void;
  callRecords: CompanionCallRecords | null;
  onOpenRecord(recordId: string): void;
}) {
  const call = snapshot?.snapshot;
  const activeCall = Boolean(call && call.telephonyState !== "ended");
  const authoritativeIdle = transport === "connected" && Boolean(idleSync);
  const desktopStateCurrent = transport === "connected" && Boolean(snapshot || idleSync);
  const onlineMembers = countAvailableRoutingMembers(bootstrap?.routingGroups ?? []);
  const recentRecords = callRecords && callRecords.access !== "none" ? callRecords.records.slice(0, 3) : null;
  const recentActivity = (bootstrap?.history.activity ?? []).slice(0, 5);
  const participantSummary = connectedParticipantSummary(call?.participants ?? []);
  return (
    <div className="screen home-screen live-workspace-screen">
      <WorkspaceHeader eyebrow={dateLabel()} title={staffGreeting(bootstrap?.staff ?? [])} action={
        <button className="icon-button" aria-label="Refresh workspace" disabled={refreshing} onClick={onRefresh}>
          <RefreshCw className={refreshing ? "spin" : ""} size={19} />
        </button>
      } />
      <section className="availability-card" aria-labelledby="live-availability-title">
        <div className="section-heading-row"><div><span className="section-kicker">YOUR AVAILABILITY</span><h2 id="live-availability-title">Ready when Aokie needs you</h2></div><span className="coverage-pill"><Users size={14} /> {onlineMembers} available</span></div>
        <div className="availability-switch" role="group" aria-label="Availability">
          {AVAILABILITY_OPTIONS.map(({ value, label }) => (
            <button
              key={value}
              className={`${bootstrap?.availability?.availability === value ? "is-active " : ""}is-${value === "do_not_disturb" ? "quiet" : value === "offline" ? "unavailable" : value}`}
              disabled={!bootstrap || availabilityBusy}
              onClick={() => onAvailability(value)}
            ><span className="availability-dot" />{label}</button>
          ))}
        </div>
        <p className="live-expiry-note"><Clock3 size={13} /> {availabilityExpiryLabel(bootstrap?.availability ?? null)}</p>
      </section>
      <section className={`line-health-card ${desktopStateCurrent ? "is-ready" : "is-warning"}`}>
        <span className="line-icon">{desktopStateCurrent ? <Wifi size={20} /> : <WifiOff size={20} />}</span>
        <div><span className="section-kicker">SECURE ROUTING</span><strong>{authoritativeIdle ? "Connected — waiting for a live Aokie call" : desktopStateCurrent ? "Desktop state and server signalling are current" : transport === "connected" ? "Server connected — waiting for Desktop state" : "Call controls are locked"}</strong><small>Audio is encrypted Desktop ↔ this mic/speaker endpoint, direct or through TURN.</small></div>
        {desktopStateCurrent && <CheckCircle2 size={20} />}
      </section>
      {activeCall ? (
        <button className="active-call-card live-authoritative-call" onClick={onOpenCall}>
          <span className="active-call-pulse"><PhoneCall size={19} /></span>
          <span><small>{call?.serviceMode.replace(/_/g, " ")}</small><strong>{call?.caller?.label ?? "Caller identity hidden"}</strong><em>{call?.caller?.maskedNumber ?? "Number permission not granted"} · {participantSummary}</em></span>
          <ChevronRight size={20} />
        </button>
      ) : (
        <section className="live-no-call"><Bot size={25} /><div><strong>{authoritativeIdle ? "Connected — waiting for a live Aokie call" : loading ? "Loading authoritative call state…" : "Aokie is ready for the next caller"}</strong><p>No microphone or speaker route is open while there is no active call.</p></div></section>
      )}
      <section className="home-recent">
        <div className="section-title-line"><h2>{recentRecords ? "Recent calls" : "Recent Companion sessions"}</h2><button onClick={onOpenHistory}>View all</button></div>
        {recentRecords?.map((record) => <CallRecordRow key={record.id} record={record} onOpen={() => onOpenRecord(record.id)} />)}
        {recentRecords && recentRecords.length === 0 && <p className="runtime-muted">No FormLogic call records are available in this view yet.</p>}
        {!recentRecords && (bootstrap?.history.sessions ?? []).slice(0, 3).map((session) => <SessionRow key={session.id} session={session} />)}
        {!recentRecords && !loading && (bootstrap?.history.sessions.length ?? 0) === 0 && <p className="runtime-muted">No Companion sessions have been recorded for this role yet.</p>}
      </section>
      <section className="home-recent" aria-labelledby="recent-companion-activity-title">
        <div className="section-title-line"><h2 id="recent-companion-activity-title">Companion activity</h2><span>{recentActivity.length} recent events</span></div>
        <CompanionActivityList activity={recentActivity} />
        {!loading && recentActivity.length === 0 && <p className="runtime-muted">No role-visible Companion activity has been returned by FormLogic yet.</p>}
      </section>
    </div>
  );
}

function normalizedRecordStatus(record: CompanionCallRecord): string {
  return record.status.trim().toLowerCase().replace(/[\s-]+/g, "_");
}

export function callRecordFilters(records: CompanionCallRecord[]): Array<{ value: CallRecordFilter; label: string }> {
  const statuses = [...new Set(records.map((record) => record.status.trim()).filter(Boolean))];
  return [
    { value: "all", label: "All" },
    { value: "follow_up", label: "Follow-up" },
    ...statuses.map((status) => ({ value: `status:${status}` as const, label: humanize(status) })),
  ];
}

export function filterCallRecords(records: CompanionCallRecord[], filter: CallRecordFilter): CompanionCallRecord[] {
  if (filter === "all") return records;
  if (filter === "follow_up") return records.filter((record) => record.followUpRequired);
  const exactStatus = filter.slice("status:".length);
  return records.filter((record) => record.status === exactStatus);
}

export function exactCallRecordId(records: CompanionCallRecords | null, callId: string | null | undefined): string | null {
  if (!records || records.access === "none" || !callId) return null;
  const exactMatches = records.records.filter((record) => record.callId === callId);
  return exactMatches.length === 1 ? exactMatches[0].id : null;
}

export function callRecordTone(record: CompanionCallRecord): "handled" | "hold" | "danger" | "private" {
  const status = normalizedRecordStatus(record);
  if (/missed|unanswered|abandoned|failed/.test(status)) return "danger";
  if (record.followUpRequired) return "hold";
  if (/takeover|human/.test(status)) return "private";
  return "handled";
}

export function HistoryScreen({ records, error, lastUpdatedAt = null, hasMore = false, loadingMore = false, onRefresh, onLoadMore = () => undefined, onOpenRecord }: {
  records: CompanionCallRecords | null;
  error: string | null;
  lastUpdatedAt?: number | null;
  hasMore?: boolean;
  loadingMore?: boolean;
  onRefresh(): void;
  onLoadMore?(): void;
  onOpenRecord(recordId: string): void;
}) {
  const [filter, setFilter] = useState<CallRecordFilter>("all");
  const [visibleLimit, setVisibleLimit] = useState(CALL_RECORD_PAGE_SIZE);
  const loadMoreSentinel = useRef<HTMLDivElement | null>(null);
  const loadRequested = useRef(false);
  const filteredRecords = useMemo(() => filterCallRecords(records?.records ?? [], filter), [filter, records]);
  const visibleRecords = useMemo(() => visibleCallRecordWindow(filteredRecords, visibleLimit), [filteredRecords, visibleLimit]);
  const bufferedRecordsRemain = visibleRecords.length < filteredRecords.length;
  const hasAdditionalRecords = bufferedRecordsRemain || hasMore;
  const filters = useMemo(() => callRecordFilters(records?.records ?? []), [records]);
  const totals = useMemo(() => ({
    records: records?.records.length ?? 0,
    inbound: records?.records.filter((record) => record.direction === "inbound").length ?? 0,
    followUps: filterCallRecords(records?.records ?? [], "follow_up").length,
  }), [records]);
  useEffect(() => {
    if (!loadingMore) loadRequested.current = false;
  }, [filter, loadingMore, records?.records.length, visibleLimit]);
  const loadNextRecords = useCallback(() => {
    if (loadingMore) return;
    if (bufferedRecordsRemain) {
      setVisibleLimit((current) => nextCallRecordLimit(current));
      return;
    }
    onLoadMore();
  }, [bufferedRecordsRemain, loadingMore, onLoadMore]);
  useEffect(() => {
    const sentinel = loadMoreSentinel.current;
    if (!sentinel || !hasAdditionalRecords || loadingMore || error || typeof IntersectionObserver === "undefined") return;
    const scrollRoot = sentinel.closest(".live-workspace-scroll");
    const observerRoot = scrollRoot instanceof HTMLElement && scrollRoot.scrollHeight > scrollRoot.clientHeight
      ? scrollRoot
      : null;
    const observer = new IntersectionObserver((entries) => {
      if (!entries.some((entry) => entry.isIntersecting) || loadRequested.current) return;
      loadRequested.current = true;
      loadNextRecords();
    }, {
      root: observerRoot,
      rootMargin: "240px 0px",
    });
    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [error, hasAdditionalRecords, loadNextRecords, loadingMore, visibleRecords.length]);
  return (
    <div className="screen calls-screen live-workspace-screen">
      <WorkspaceHeader eyebrow="FORMLOGIC RECORDS" title="Calls" action={<button className="icon-button" aria-label="Refresh call records" onClick={onRefresh}><RefreshCw size={19} /></button>} />
      {error && records && <div className="live-stale-records" role="alert"><AlertTriangle size={18} /><div><strong>Showing the last successful FormLogic response</strong><p>{error}{lastUpdatedAt ? ` · Last refreshed ${new Date(lastUpdatedAt).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}` : ""}</p></div></div>}
      {records?.access === "none" ? (
        <div className="empty-state live-role-empty"><LockKeyhole size={30} /><strong>Call records are not available to this role</strong><p>FormLogic did not grant call-record access to this Companion role or installed pack. Companion media history remains audited separately.</p></div>
      ) : records ? (
        <>
          <div className="history-summary"><div><strong>{totals.records}</strong><span>{hasAdditionalRecords ? "Loaded" : "Records"}</span></div><div><strong>{totals.inbound}</strong><span>{hasAdditionalRecords ? "Inbound shown" : "Inbound"}</span></div><div><strong>{totals.followUps}</strong><span>{hasAdditionalRecords ? "Follow-up shown" : "Follow-up"}</span></div></div>
          <div className="filter-scroller" role="group" aria-label="Filter FormLogic call records">{filters.map((option) => <button key={option.value} className={filter === option.value ? "is-active" : ""} aria-pressed={filter === option.value} onClick={() => { setFilter(option.value); setVisibleLimit(CALL_RECORD_PAGE_SIZE); }}>{option.label}</button>)}</div>
          <section className="call-history-list"><div className="section-title-line"><h2>{filters.find((option) => option.value === filter)?.label ?? "Calls"}</h2><span>{loadedCallRecordCountLabel(visibleRecords.length, hasAdditionalRecords)}</span></div>
            {visibleRecords.map((record) => <CallRecordRow key={record.id} record={record} onOpen={() => onOpenRecord(record.id)} />)}
            {visibleRecords.length === 0 && <div className="empty-state"><History size={28} /><strong>No records in this view</strong><p>Choose another filter or refresh the role-filtered FormLogic records.</p></div>}
            {(hasAdditionalRecords || loadingMore) && <div ref={loadMoreSentinel} className="call-history-sentinel" aria-live="polite" aria-busy={loadingMore}>
              {loadingMore ? <><RefreshCw className="spin" size={18} /><span><strong>Loading older calls</strong><small>Fetching the next {CALL_RECORD_PAGE_SIZE} role-visible records…</small></span></> : <button type="button" onClick={loadNextRecords}><History size={16} /><span><strong>{error ? "Try loading older calls" : "Load older calls"}</strong><small>More records load automatically as you scroll</small></span></button>}
            </div>}
            {!hasAdditionalRecords && !loadingMore && visibleRecords.length > 0 && <div className="call-history-end"><CheckCircle2 size={15} /><span>{(records?.records.length ?? 0) >= CALL_RECORD_MAX_LIMIT ? `Latest ${CALL_RECORD_MAX_LIMIT} records loaded` : "All available records loaded"}</span></div>}
          </section>
        </>
      ) : (
        <div className="empty-state"><RefreshCw className={!error ? "spin" : ""} size={28} /><strong>{error ? "Call records could not be loaded" : "Loading call records"}</strong><p>{error ?? "Waiting for the role-filtered FormLogic response."}</p></div>
      )}
    </div>
  );
}

function CallRecordRow({ record, onOpen }: { record: CompanionCallRecord; onOpen(): void }) {
  const tone = callRecordTone(record);
  return (
    <button className="history-card live-call-record" onClick={onOpen}>
      <span className={`result-icon is-${tone}`}><PhoneCall size={17} /></span>
      <span className="history-card-copy">
        <strong>{record.callerName ?? "Caller identity hidden"}</strong>
        <small>{record.maskedNumber ?? "Number unavailable"} · {formatDateTime(record.startedAt ?? record.submittedAt)}{record.durationSeconds === null ? "" : ` · ${formatCallDuration(record.durationSeconds)}`}</small>
        <span className="record-status-line"><em className={`is-${tone}`}>{humanize(record.status)}</em>{record.followUpRequired && <b>Follow-up</b>}</span>
      </span>
      <ChevronRight size={18} />
    </button>
  );
}

export function CallDetailScreen({ detail, loading, error, activity = [], onBack }: {
  detail: CompanionCallRecordDetail | null;
  loading: boolean;
  error: string | null;
  activity?: CompanionActivity[];
  onBack(): void;
}) {
  if (loading || !detail) {
    return (
      <div className="screen detail-screen live-workspace-screen">
        <header className="detail-header"><button className="icon-button" aria-label="Back to calls" onClick={onBack}><ArrowLeft size={20} /></button><div><span className="section-kicker">CALL RECORD</span><h1>Call details</h1></div><span /></header>
        <div className="empty-state"><RefreshCw className={loading ? "spin" : ""} size={28} /><strong>{error ? "Call record unavailable" : "Loading call record"}</strong><p>{error ?? "Waiting for the role-filtered FormLogic record."}</p></div>
      </div>
    );
  }
  const { record, transcript, followUps } = detail;
  const tone = callRecordTone(record);
  return (
    <div className="screen detail-screen live-workspace-screen">
      <header className="detail-header"><button className="icon-button" aria-label="Back to calls" onClick={onBack}><ArrowLeft size={20} /></button><div><span className="section-kicker">CALL RECORD</span><h1>{record.callerName ?? "Caller identity hidden"}</h1></div><span /></header>
      <section className="detail-hero">
        <span className={`result-icon large is-${tone}`}><PhoneCall size={22} /></span>
        <h2>{humanize(record.status)}</h2>
        <p>{formatDateTime(record.startedAt ?? record.submittedAt)} · {record.maskedNumber ?? "Number unavailable"}</p>
        <div className="detail-tags"><span><PhoneCall size={13} /> {humanize(record.direction)}</span>{record.durationSeconds !== null && <span><Clock3 size={13} /> {formatCallDuration(record.durationSeconds)}</span>}{record.followUpRequired && <span><AlertTriangle size={13} /> Follow-up required</span>}</div>
      </section>
      <section className="detail-section"><span className="section-kicker">SUMMARY</span><p className="summary-copy">{record.summary ?? "No call summary is available to this role."}</p></section>
      {activity.length > 0 && <section className="detail-section"><div className="section-title-line"><h2>Companion activity</h2><span>{activity.length} recent events</span></div><CompanionActivityList activity={activity} /></section>}
      <section className="detail-section"><div className="section-title-line"><h2>Transcript</h2><span>{transcript.length} entries</span></div>
        {transcript.map((entry) => <article className="mini-transcript live-record-transcript" key={entry.id}><div><strong>{entry.speaker}</strong><time>{formatDateTime(entry.occurredAt)}</time></div><p>{entry.text}</p></article>)}
        {transcript.length === 0 && <p className="runtime-muted">No transcript is available to this role or record.</p>}
      </section>
      <section className="detail-section"><div className="section-title-line"><h2>Follow-up</h2><span>{followUps.length} items</span></div>
        {followUps.map((followUp) => <article className="live-follow-up" key={followUp.id}><span><MessageSquare size={15} /></span><div><strong>{followUp.summary}</strong><small>{humanize(followUp.priority)} · {formatDateTime(followUp.submittedAt)}</small></div><em>{humanize(followUp.status)}</em></article>)}
        {followUps.length === 0 && <p className="runtime-muted">No follow-up items are attached to this call.</p>}
      </section>
    </div>
  );
}

export function CompanionActivityList({ activity }: { activity: CompanionActivity[] }) {
  return (
    <div className="live-activity-list">
      {activity.map((item) => (
        <article key={item.id}>
          <History size={17} />
          <span><strong>{humanize(item.eventType)}</strong><small>{[item.mode ? humanize(item.mode) : null, formatDateTime(item.occurredAt)].filter(Boolean).join(" · ")}</small></span>
        </article>
      ))}
    </div>
  );
}

function TeamScreen({ groups, staff }: { groups: CompanionRoutingGroupView[]; staff: CompanionStaffMember[] }) {
  return (
    <div className="screen team-screen live-workspace-screen">
      <WorkspaceHeader eyebrow="SERVER-ENFORCED ROUTING" title="Team & routing" />
      <section className="live-staff-directory" aria-labelledby="staff-directory-title">
        <div className="section-title-line"><h2 id="staff-directory-title">FormLogic staff</h2><span>{staffCountLabel(staff.length)}</span></div>
        {staff.map((member) => <article className="live-staff-card" key={member.id}><span className="member-avatar">{initials(member.displayName)}</span><div className="member-copy"><strong>{member.displayName}{member.isCurrentUser && <em>You</em>}</strong><small>{member.roleName}</small><span>{member.companionReady ? "Companion ready" : "Not enrolled for Companion"}</span></div><div className="staff-badges">{member.isOwner && <span>Owner</span>}<span className={member.companionReady ? "is-ready" : "is-neutral"}>{member.companionReady ? "Ready" : "Directory"}</span></div></article>)}
        {staff.length === 0 && <div className="empty-state"><Users size={28} /><strong>No staff directory available</strong><p>FormLogic did not return staff records for this membership.</p></div>}
      </section>
      {groups.map((group) => <section className="live-routing-group" key={group.id}><header><span className="routing-icon"><Users size={20} /></span><div><strong>{group.name}</strong><small>{humanize(group.policy)} · {group.enabled ? "enabled" : "disabled"}</small></div></header>{sortRoutingMembers(group.members).map((member) => <article className={`member-card live-routing-member ${member.enabled ? "" : "is-disabled"}`} key={`${group.id}-${member.staffId ?? member.displayName}-${member.priority}`}><span className="routing-priority" aria-label={`Priority ${member.priority}`}>{member.priority}</span><span className={`member-avatar ${member.availability === "offline" ? "is-offline" : ""}`}>{initials(member.displayName)}</span><div className="member-copy"><strong>{member.displayName}{member.isCurrentUser && <em>You</em>}</strong><small>{member.roleName ?? "Role unavailable"} · {humanize(member.availability)}</small><span>{member.availabilityExpiresAt ? `Until ${formatDateTime(member.availabilityExpiresAt)}` : "Server-managed availability"}</span></div><div className="routing-member-state"><span className={`live-member-dot is-${member.availability}`} /><em>{member.enabled ? "Included" : "Disabled"}</em></div></article>)}</section>)}
      {groups.length === 0 && <div className="empty-state"><Users size={28} /><strong>No routing group assigned</strong><p>An owner can configure teams and permissions in FormLogic.</p></div>}
      <aside className="web-permissions-note"><ShieldCheck size={18} /><div><strong>Routing is read-only in Companion</strong><p>Owners manage staff roles, Companion enrollment, priority order, and alert routing in FormLogic. This app cannot grant itself access or change the queue.</p></div></aside>
    </div>
  );
}

export function SettingsScreen({ bootstrap, staff, runtime, transport, audioDevices, audioBusy, audioError, serverProfiles, serverProfilesError, onAudioSelection, onRefresh, onDisconnect, onForgetManaged, onRunSetup }: {
  bootstrap: CompanionBootstrapView | null;
  staff: CompanionStaffMember[];
  runtime: RuntimeCapabilities;
  transport: Transport;
  audioDevices: NativeAudioDevices | null;
  audioBusy: boolean;
  audioError: string | null;
  serverProfiles: ServerProfile[] | null;
  serverProfilesError: string | null;
  onAudioSelection(inputId: string, outputId: string): Promise<void>;
  onRefresh(): void;
  onDisconnect(): Promise<void>;
  onForgetManaged(): Promise<void>;
  onRunSetup?: () => void | Promise<void>;
}) {
  const [forgetOpen, setForgetOpen] = useState(false);
  const [forgetBusy, setForgetBusy] = useState(false);
  const [forgetError, setForgetError] = useState<string | null>(null);
  const currentStaff = findCurrentStaff(staff);
  const accountName = currentStaff?.displayName ?? bootstrap?.device.displayName ?? "Aokie Companion";
  const accountRole = currentStaff?.roleName ?? bootstrap?.device.role ?? "mobile";
  return (
    <div className="screen settings-screen live-workspace-screen">
      <WorkspaceHeader eyebrow="ENROLLED ENDPOINT" title="Settings" action={<button className="icon-button" aria-label="Refresh settings" onClick={onRefresh}><RefreshCw size={19} /></button>} />
      <section className="account-card"><span className="member-avatar">{initials(accountName)}</span><div><strong>{accountName}</strong><small>{accountRole}{currentStaff?.isOwner ? " · Owner" : ""} · {bootstrap?.membership.appSlug ?? "custom deployment"}</small></div><CheckCircle2 size={18} /></section>
      <section className="settings-section"><span className="section-kicker">CONNECTION</span>
        <SettingsRow icon={Server} label="Control server" detail="FormLogic or a compatible custom server" value={bootstrap?.membership.status ?? "Loading"} />
        <SettingsRow icon={Wifi} label="Realtime signalling" detail="No unencrypted call audio enters the web server" value={transport} />
        <SettingsRow icon={LockKeyhole} label="Secure media" detail="WebRTC DTLS-SRTP, direct or encrypted TURN" value={runtime.mediaBridge ? "Ready" : "Locked"} />
        <SettingsRow icon={Smartphone} label="Audio endpoint" detail="This device’s microphone and speakers; no dongle pairing" value={runtime.platform} />
        <SettingsRow icon={LockKeyhole} label="Native secure storage" detail="Credentials stay outside the Companion renderer" value={runtime.secureStorage ? "Ready" : "Unavailable"} />
      </section>
      <ServerProfileSettings profiles={serverProfiles} error={serverProfilesError} />
      <AudioEndpointSettings devices={audioDevices} busy={audioBusy} error={audioError} onSelection={onAudioSelection} />
      <section className="settings-section"><span className="section-kicker">PERMISSIONS &amp; BACKGROUND</span>
        <SettingsRow icon={Bell} label="Notifications" detail="Required for background call and help offers on supported mobile builds" value={runtime.notificationPermission} />
        <SettingsRow icon={Mic} label="Microphone" detail="Requested only for a proven consult or takeover route" value={runtime.microphonePermission} />
        <SettingsRow icon={Smartphone} label="Call infrastructure" detail="Platform-reported foreground or native call integration" value={runtime.callInfrastructure} />
        <SettingsRow icon={Bell} label="Push registration" detail={runtime.fcmConfigured ? "Native provider configuration is present" : "No Firebase provider configuration is reported"} value={runtime.pushRegistration} />
        <SettingsRow icon={Smartphone} label="Battery optimisation" detail="Restricted background execution can delay alerts; force-stop cannot be bypassed" value={runtime.batteryOptimizationsRestricted ? "Restricted" : "Normal"} />
        <SettingsRow icon={PhoneCall} label="Pending native offer" detail="A native call surface still re-fetches and verifies the authoritative offer" value={runtime.pendingCallOffer ? "Pending" : "None"} />
        <SettingsRow icon={ShieldCheck} label="Force-stop visibility" detail="The operating system may not expose or recover a force-stopped app" value={runtime.forceStopState} />
      </section>
      <aside className="live-media-boundary"><LockKeyhole size={18} /><div><strong>Encrypted media boundary</strong><p>Caller audio is encrypted between Aokie Desktop and this enrolled Companion endpoint. The carrier and Bluetooth legs stay outside that boundary.</p></div></aside>
      {runtime.lastNativeDiagnostic && <aside className="live-native-diagnostic" role="status"><AlertTriangle size={18} /><div><strong>Latest native diagnostic</strong><p>{runtime.lastNativeDiagnostic}</p></div></aside>}
      <section className="settings-section"><span className="section-kicker">CURRENT CAPABILITIES</span><div className="live-capability-list">{(bootstrap?.capabilities ?? []).map((capability) => <span key={capability}><ShieldCheck size={13} />{humanize(capability)}</span>)}{bootstrap?.capabilities.length === 0 && <p className="runtime-muted">No remote audio capability is currently granted.</p>}</div></section>
      <section className="settings-section"><span className="section-kicker">THIS DEVICE</span><SettingsRow icon={ShieldCheck} label="Server approval" detail={bootstrap?.device.revokedAt ? "Revoked" : `Approved ${formatDateTime(bootstrap?.device.approvedAt)}`} value={bootstrap?.device.revokedAt ? "Locked" : "Active"} /><SettingsRow icon={Clock3} label="Last server activity" detail="Role-filtered device presence" value={formatDateTime(bootstrap?.device.lastSeenAt)} /><SettingsRow icon={Bell} label="Push endpoints" detail="Only redacted fingerprints are returned to the app UI" value={String(bootstrap?.pushEndpoints.length ?? 0)} /></section>
      {onRunSetup && <button className="settings-action" onClick={() => void onRunSetup()}><RefreshCw size={17} /><span><strong>Run setup checks again</strong><small>Review native permissions, pairing, trust, and connection readiness.</small></span><ChevronRight size={18} /></button>}
      <button className="settings-action" onClick={() => void onDisconnect()}><WifiOff size={17} /><span><strong>Disconnect this session</strong><small>Closes realtime and all native media; saved sign-in remains until removed.</small></span><ChevronRight size={18} /></button>
      <button className="settings-action is-danger" onClick={() => { setForgetError(null); setForgetOpen(true); }}><LockKeyhole size={17} /><span><strong>Sign out and remove this device</strong><small>Revokes this native session, unregisters push, and deletes saved credentials from this endpoint.</small></span><ChevronRight size={18} /></button>
      {forgetOpen && <div className="modal-layer is-centered live-forget-confirmation" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget && !forgetBusy) setForgetOpen(false); }}><section className="modal-sheet is-dialog" role="dialog" aria-modal="true" aria-labelledby="forget-device-title"><div className="danger-modal-icon"><LockKeyhole size={24} /></div><h2 id="forget-device-title">Remove this Companion sign-in?</h2><p className="modal-lead">All call media closes first. Native credentials and registered push delivery for this installation are removed; an owner can separately revoke the device record in FormLogic.</p>{forgetError && <div className="setup-result is-failed" role="alert"><WifiOff size={16} /><span>{forgetError}</span></div>}<button className="danger-button" disabled={forgetBusy} onClick={() => { setForgetBusy(true); setForgetError(null); void onForgetManaged().catch((caught) => { setForgetError(displayError(caught, "This device could not be removed")); setForgetBusy(false); }); }}>{forgetBusy ? "Removing device…" : "Sign out and remove"}</button><button className="text-button" disabled={forgetBusy} onClick={() => setForgetOpen(false)}>Keep this device</button></section></div>}
    </div>
  );
}

export function ServerProfileSettings({ profiles, error }: { profiles: ServerProfile[] | null; error: string | null }) {
  const activeProfile = activeTrustedServerProfile(profiles);
  return (
    <section className="settings-section server-profile-settings">
      <span className="section-kicker">TRUSTED SERVER</span>
      {activeProfile ? (
        <>
          <SettingsRow icon={Server} label="Active origin" detail={activeProfile.origin} value="Trusted" />
          <SettingsRow icon={ShieldCheck} label="Deployment" detail={activeProfile.deploymentId} value="Active" />
          <SettingsRow icon={LockKeyhole} label="Discovery fingerprint" detail={boundedFingerprint(activeProfile.discoveryFingerprint)} value="Verified" />
          <SettingsRow icon={LockKeyhole} label="Endpoint fingerprint" detail={boundedFingerprint(activeProfile.endpointFingerprint)} value="Verified" />
        </>
      ) : (
        <p className="runtime-muted">{profiles === null ? "Loading the active native trust profile…" : "No single active trusted server profile is available on this endpoint."}</p>
      )}
      {error && <div className="runtime-error" role="alert"><WifiOff size={16} /><span>{error}</span></div>}
    </section>
  );
}

export function AudioEndpointSettings({ devices, busy, error = null, onSelection }: {
  devices: NativeAudioDevices | null;
  busy: boolean;
  error?: string | null;
  onSelection(inputId: string, outputId: string): Promise<void>;
}) {
  if (!devices) {
    return <section className="settings-section audio-endpoint-settings"><span className="section-kicker">MICROPHONE &amp; SPEAKERS</span><p className="runtime-muted">{error ?? "Open this view or refresh Settings to enumerate this endpoint's native audio devices."}</p></section>;
  }
  if (devices.routingPolicy === "system_managed") {
    const selectionLocked = busy || !devices.canSelect;
    return (
      <section className="settings-section audio-endpoint-settings">
        <span className="section-kicker">CALL AUDIO ROUTE</span>
        {error && <div className="runtime-error" role="alert"><WifiOff size={16} /><span>{error}</span></div>}
        {devices.outputDevices.length > 0 ? (
          <label className="audio-endpoint-field"><span><Smartphone size={16} /><strong>Speaker, earpiece, or headset</strong></span><select aria-label="Companion call audio route" value={devices.selectedOutputId} disabled={selectionLocked} onChange={(event) => void onSelection(event.target.value, event.target.value)}>{devices.selectedOutputId === "system_managed" && <option value="system_managed">System default</option>}{devices.outputDevices.map((device) => <option key={device.id} value={device.id}>{device.label}</option>)}</select></label>
        ) : <SettingsRow icon={Smartphone} label="System-managed audio route" detail="No selectable communication route is currently reported by the operating system." value="Native" />}
        <p className="runtime-muted">{devices.state === "idle" ? "Android enables route selection after authenticated call media starts." : "This changes this Companion endpoint's Android communication route only; it never controls the Aokie Bluetooth dongle."}</p>
      </section>
    );
  }
  if (devices.routingPolicy !== "selectable") {
    return <section className="settings-section audio-endpoint-settings"><span className="section-kicker">MICROPHONE &amp; SPEAKERS</span>{error && <div className="runtime-error" role="alert"><WifiOff size={16} /><span>{error}</span></div>}<p className="runtime-muted">Native audio-device selection is unavailable in this runtime.</p></section>;
  }
  const selectionLocked = busy || !devices.canSelect || devices.state !== "idle";
  return (
    <section className="settings-section audio-endpoint-settings">
      <span className="section-kicker">MICROPHONE &amp; SPEAKERS</span>
      {error && <div className="runtime-error" role="alert"><WifiOff size={16} /><span>{error}</span></div>}
      <label className="audio-endpoint-field"><span><Mic size={16} /><strong>Microphone</strong></span><select aria-label="Companion microphone" value={devices.selectedInputId} disabled={selectionLocked} onChange={(event) => void onSelection(event.target.value, devices.selectedOutputId)}>{devices.inputDevices.map((device) => <option key={device.id} value={device.id}>{device.label}</option>)}</select></label>
      <label className="audio-endpoint-field"><span><Headphones size={16} /><strong>Speakers or headset</strong></span><select aria-label="Companion speakers" value={devices.selectedOutputId} disabled={selectionLocked} onChange={(event) => void onSelection(devices.selectedInputId, event.target.value)}>{devices.outputDevices.map((device) => <option key={device.id} value={device.id}>{device.label}</option>)}</select></label>
      <p className="runtime-muted">{devices.state === "idle" ? "These are this Companion endpoint's devices, not the Aokie Bluetooth dongle." : "Device changes are locked while call media is active. Return or stop media before changing endpoints."}</p>
    </section>
  );
}

function WorkspaceHeader({ eyebrow, title, action }: { eyebrow: string; title: string; action?: ReactNode }) {
  return <header className="app-header"><div><span className="eyebrow">{eyebrow}</span><h1>{title}</h1></div>{action ?? <span className="live-trust-badge"><ShieldCheck size={16} /> Live</span>}</header>;
}

function SessionRow({ session, expanded = false }: { session: CompanionSessionView; expanded?: boolean }) {
  return <article className={`history-card live-session-row ${expanded ? "is-expanded" : ""}`}><span className={`result-icon is-${session.mode === "takeover" ? "danger" : session.mode === "consult" ? "private" : "handled"}`}><Headphones size={17} /></span><span className="history-card-copy"><strong>{humanize(session.mode)} session</strong><small>{formatDateTime(session.joinedAt ?? session.lastEventAt)} · Call …{session.callId.slice(-6)}</small><em>{humanize(session.state)}</em>{expanded && session.endReason && <span className="live-end-reason">{humanize(session.endReason)}</span>}</span></article>;
}

function SettingsRow({ icon: Icon, label, detail, value }: { icon: LucideIcon; label: string; detail: string; value: string }) {
  const tone = settingsValueTone(value);
  return <div className="settings-row"><span className="settings-row-icon"><Icon /></span><span><strong>{label}</strong><small>{detail}</small></span><em className={`is-${tone}`}>{humanize(value)}{tone === "ready" ? <CheckCircle2 size={14} /> : tone === "warning" ? <AlertTriangle size={14} /> : null}</em></div>;
}

export function activeTrustedServerProfile(profiles: ServerProfile[] | null): ServerProfile | null {
  if (!profiles) return null;
  const active = profiles.filter((profile) => profile.active && profile.trustState === "trusted");
  return active.length === 1 ? active[0] : null;
}

export function boundedFingerprint(value: string, maxLength = 24): string {
  const normalized = value.trim();
  const boundedLength = Math.max(12, Math.min(64, Math.floor(maxLength)));
  if (normalized.length <= boundedLength) return normalized;
  const visibleLength = boundedLength - 1;
  const prefixLength = Math.ceil(visibleLength / 2);
  const suffixLength = visibleLength - prefixLength;
  return `${normalized.slice(0, prefixLength)}…${normalized.slice(-suffixLength)}`;
}

export function connectedParticipantCounts(participants: ReadonlyArray<{ mode: string; state: string }>): { connected: number; observers: number } {
  const current = participants.filter((participant) => participant.state === "connected" || participant.state === "active");
  return {
    connected: current.length,
    observers: current.filter((participant) => participant.mode === "observer").length,
  };
}

export function connectedParticipantSummary(participants: ReadonlyArray<{ mode: string; state: string }>): string {
  const { connected, observers } = connectedParticipantCounts(participants);
  return `${connected} connected ${connected === 1 ? "participant" : "participants"} · ${observers} ${observers === 1 ? "observer" : "observers"}`;
}

export function settingsValueTone(value: string): "ready" | "warning" | "neutral" {
  const normalized = value.trim().toLowerCase().replace(/[\s-]+/g, "_");
  if (/denied|unavailable|locked|offline|revoked|failed|configuration_required/.test(normalized)) return "warning";
  if (/^(ready|active|connected|granted|registered|native|full|own|trusted|verified)$/.test(normalized)) return "ready";
  return "neutral";
}

export function sortRoutingMembers(members: CompanionRoutingGroup["members"]): CompanionRoutingGroup["members"] {
  return [...members].sort((left, right) => left.priority - right.priority || left.displayName.localeCompare(right.displayName));
}

export function findCurrentStaff(staff: CompanionStaffMember[]): CompanionStaffMember | null {
  return staff.find((member) => member.isCurrentUser) ?? null;
}

export function staffGreeting(staff: CompanionStaffMember[], hour = new Date().getHours()): string {
  const current = findCurrentStaff(staff);
  if (!current) return "Aokie Companion";
  const firstName = current.displayName.trim().split(/\s+/)[0] || current.displayName;
  const period = hour < 12 ? "morning" : hour < 18 ? "afternoon" : "evening";
  return `Good ${period}, ${firstName}`;
}

export function staffCountLabel(count: number): string {
  return `${count} ${count === 1 ? "person" : "people"}`;
}

export function countAvailableRoutingMembers(groups: CompanionRoutingGroup[]): number {
  const available = new Set<string>();
  for (const group of groups) {
    if (!group.enabled) continue;
    for (const member of group.members) {
      if (member.enabled && member.availability === "available") available.add(member.staffId ?? `name:${member.displayName}`);
    }
  }
  return available.size;
}

export function availabilityExpiryLabel(availability: CompanionAvailabilityRecord | null, nowMs = Date.now()): string {
  if (!availability) return "Choose a server-enforced status; each selection expires automatically.";
  if (!availability.expiresAt) return `${humanize(availability.availability)} has no expiry reported by the server.`;
  const expiresAt = Date.parse(availability.expiresAt);
  if (!Number.isFinite(expiresAt)) return `${humanize(availability.availability)} has an unreadable server expiry.`;
  if (expiresAt <= nowMs) return `${humanize(availability.availability)} expired; refresh before relying on alert routing.`;
  return `${humanize(availability.availability)} until ${formatDateTime(availability.expiresAt)}.`;
}

function formatCallDuration(seconds: number): string {
  const safe = Math.max(0, Math.floor(seconds));
  const minutes = Math.floor(safe / 60);
  const remaining = safe % 60;
  return `${minutes}:${remaining.toString().padStart(2, "0")}`;
}

function humanize(value: string): string {
  return value.replace(/_/g, " ").replace(/\b\w/g, (letter: string) => letter.toUpperCase());
}

function initials(value: string): string {
  const parts = value.trim().split(/\s+/).filter(Boolean);
  return (parts.length > 1 ? `${parts[0][0]}${parts[parts.length - 1]?.[0] ?? ""}` : value.slice(0, 2)).toUpperCase();
}

function formatDateTime(value: string | null | undefined): string {
  if (!value) return "Not yet";
  const parsed = new Date(value.includes("T") ? value : value.replace(" ", "T") + "Z");
  return Number.isFinite(parsed.getTime()) ? parsed.toLocaleString([], { dateStyle: "medium", timeStyle: "short" }) : "Recorded";
}

function dateLabel(): string {
  return new Date().toLocaleDateString([], { weekday: "long", day: "numeric", month: "long" }).toUpperCase();
}
