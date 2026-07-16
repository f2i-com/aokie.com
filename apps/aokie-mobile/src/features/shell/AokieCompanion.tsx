import {
  AlertTriangle, ArrowDown, ArrowLeft, ArrowUp, BadgeCheck, Bell, Bot, Check,
  CheckCircle2, ChevronRight, CircleDot, Clock3, Cloud, Database, Ellipsis,
  ExternalLink, Eye, Gauge, GripVertical, Headphones, HelpCircle, History, Home,
  Info, KeyRound, ListFilter, LockKeyhole, MessageSquare, Mic, MicOff, Monitor,
  Network, Phone, PhoneCall, PhoneIncoming, PhoneOff, Radio, RefreshCw, RotateCcw,
  Router, ScanLine, Send, Server, Settings, ShieldCheck, Smartphone, Sparkles,
  UserCheck, UserRound, Users, Volume2, Wifi, WifiOff, X, type LucideIcon,
} from "lucide-react";
import {
  useEffect, useMemo, useReducer, useRef, useState,
  type CSSProperties, type Dispatch, type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";
import { business, callHistory, currentCall, healthItems, scenarios, teamMembers, transcript } from "../demo/data";
import {
  createInitialState, prototypeReducer, type Availability, type PrototypeAction,
  type PrototypeState, type Scenario, type Screen,
} from "../../state/prototypeModel";

type AppDispatch = Dispatch<PrototypeAction>;
const cn = (...classes: Array<string | false | null | undefined>) => classes.filter(Boolean).join(" ");
const navItems: Array<{ id: Screen; label: string; icon: LucideIcon }> = [
  { id: "home", label: "Home", icon: Home },
  { id: "calls", label: "Calls", icon: PhoneCall },
  { id: "team", label: "Team", icon: Users },
  { id: "settings", label: "Settings", icon: Settings },
];
const filterOptions = ["All", "Aokie handled", "Taken over", "Missed", "Callback"];

export default function AokieCompanion() {
  const [state, dispatch] = useReducer(prototypeReducer, undefined, createInitialState);
  const [captureMode, setCaptureMode] = useState(false);

  useEffect(() => {
    const timer = window.setTimeout(() => {
      const params = new URLSearchParams(window.location.search);
      setCaptureMode(params.get("capture") === "1");
      dispatch({ type: "SET_STATIC_MODE", value: params.get("static") === "1" });
      const scenario = params.get("scenario") as Scenario | null;
      if (["aokie","listening","help","pending","human","recovery","second-caller","offline","revoked","ended"].includes(scenario ?? "")) {
        dispatch({ type: "SET_SCENARIO", scenario: scenario as Scenario });
      }
      const screen = params.get("screen") as Screen | null;
      if (["onboarding","home","live","calls","call-detail","team","settings"].includes(screen ?? "")) {
        dispatch({ type: "NAVIGATE", screen: screen as Screen });
      }
      const step = Number(params.get("step"));
      if (Number.isInteger(step) && step >= 0 && step <= 3) dispatch({ type: "SET_ONBOARDING_STEP", step });
    }, 0);
    return () => window.clearTimeout(timer);
  }, []);

  useEffect(() => {
    if (!state.toast) return;
    const timer = window.setTimeout(() => dispatch({ type: "CLEAR_TOAST" }), 2800);
    return () => window.clearTimeout(timer);
  }, [state.toast]);

  useEffect(() => {
    if (state.staticMode || state.serviceMode !== "human_pending") return;
    const timer = window.setTimeout(() => dispatch({ type: "ADVANCE_PENDING" }), 780);
    return () => window.clearTimeout(timer);
  }, [state.pendingStep, state.serviceMode, state.staticMode]);

  useEffect(() => {
    if (state.staticMode || state.serviceMode !== "returning_to_aokie") return;
    const timer = window.setTimeout(() => dispatch({ type: "FINISH_RETURN" }), 1500);
    return () => window.clearTimeout(timer);
  }, [state.serviceMode, state.staticMode]);

  useEffect(() => {
    if (state.staticMode || state.serviceMode !== "recovering") return;
    if (state.recoveryCountdown <= 0) {
      const fallback = window.setTimeout(() => dispatch({ type: "RECOVERY_FALLBACK" }), 700);
      return () => window.clearTimeout(fallback);
    }
    const timer = window.setTimeout(() => dispatch({ type: "RECOVERY_TICK" }), 1000);
    return () => window.clearTimeout(timer);
  }, [state.recoveryCountdown, state.serviceMode, state.staticMode]);

  useEffect(() => {
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (state.endCallOpen) dispatch({ type: "CLOSE_END_CALL" });
      else if (state.takeoverOpen) dispatch({ type: "CLOSE_TAKEOVER" });
      else if (state.helpOpen) dispatch({ type: "CLOSE_HELP" });
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [state.endCallOpen, state.helpOpen, state.takeoverOpen]);

  return (
    <main className={cn("prototype-root", captureMode && "capture-mode")}>
      <DesktopHeader />
      <div className="prototype-workbench">
        <section className="app-frame" aria-label="Aokie Companion mobile prototype">
          <div className="mobile-safe-top" aria-hidden="true"><span>6:42</span><div className="device-status"><Wifi size={13} /><span className="battery" /></div></div>
          <div className="screen-stack">{renderScreen(state, dispatch)}</div>
          {state.screen !== "live" && state.screen !== "onboarding" && !state.callEnded && <PersistentCallCapsule dispatch={dispatch} />}
          {state.screen !== "onboarding" && <MobileNav screen={state.screen} dispatch={dispatch} />}
          {state.toast && <Toast message={state.toast} />}
        </section>
        <ScenarioPanel state={state} dispatch={dispatch} />
      </div>
    </main>
  );
}

function renderScreen(state: PrototypeState, dispatch: AppDispatch) {
  switch (state.screen) {
    case "onboarding": return <OnboardingScreen state={state} dispatch={dispatch} />;
    case "live": return <LiveCallScreen state={state} dispatch={dispatch} />;
    case "calls": return <CallsScreen state={state} dispatch={dispatch} />;
    case "call-detail": return <CallDetailScreen state={state} dispatch={dispatch} />;
    case "team": return <TeamScreen />;
    case "settings": return <SettingsScreen state={state} dispatch={dispatch} />;
    default: return <HomeScreen state={state} dispatch={dispatch} />;
  }
}

function DesktopHeader() {
  return (
    <header className="desktop-header">
      <div className="brand-lockup"><AokieMark /><div><strong>Aokie Companion</strong><span className="demo-badge">DEMO PROTOTYPE</span></div></div>
      <div className="desktop-user"><span className="secure-channel"><ShieldCheck size={15} /> Secure demo channel</span><span className="online-dot" /><span className="user-avatar">LB</span></div>
    </header>
  );
}

function AokieMark({ compact = false }: { compact?: boolean }) {
  return <span className={cn("aokie-mark", compact && "is-compact")} aria-hidden="true">{[8,18,29,16,25,12,20].map((height,index) => <i key={`${height}-${index}`} style={{ height }} />)}</span>;
}

function AppHeader({ eyebrow, title, action }: { eyebrow?: string; title: string; action?: ReactNode }) {
  return <header className="app-header"><div>{eyebrow && <span className="eyebrow">{eyebrow}</span>}<h1>{title}</h1></div>{action ?? <button className="avatar-button" aria-label="Open profile">LB</button>}</header>;
}

function HomeScreen({ state, dispatch }: { state: PrototypeState; dispatch: AppDispatch }) {
  return (
    <div className="screen home-screen">
      <AppHeader eyebrow="Wednesday · 15 July" title="Good evening, Lance" />
      <section className="availability-card" aria-labelledby="availability-title">
        <div className="section-heading-row"><div><span className="section-kicker">YOUR AVAILABILITY</span><h2 id="availability-title">Ready when Aokie needs you</h2></div><span className="coverage-pill"><Users size={14} /> 3 online</span></div>
        <div className="availability-switch" role="group" aria-label="Availability">
          {(["available","quiet","unavailable"] as Availability[]).map((availability) => <button key={availability} className={cn(state.availability === availability && "is-active", `is-${availability}`)} onClick={() => dispatch({ type: "SET_AVAILABILITY", availability })}><span className="availability-dot" />{availability[0].toUpperCase() + availability.slice(1)}</button>)}
        </div>
        <p className="micro-copy"><Clock3 size={13} /> Available for alerts until 8:00 pm</p>
      </section>
      <section className="line-health-card"><div className="line-health-icon"><Radio size={22} /></div><div className="line-health-copy"><div><span className="status-dot" /> Aokie line ready</div><p>Desktop, business phone and secure media are connected.</p></div><button aria-label="View connection details" onClick={() => dispatch({ type: "NAVIGATE", screen: "settings" })}><ChevronRight size={19} /></button></section>
      <section className="active-call-card">
        <div className="call-card-topline"><span className="live-chip"><span /> LIVE CALL</span><span className="timer"><CircleDot size={14} /> {currentCall.duration}</span></div>
        <div className="caller-hero"><span className="caller-avatar">MT</span><div><h2>{currentCall.callerName}</h2><p>{currentCall.maskedNumber}</p></div><AokieWave /></div>
        <p className="call-intent">{currentCall.intent}</p>
        <div className="handling-row"><span><Bot size={15} /> Aokie is speaking</span><span><Eye size={15} /> 2 observing</span></div>
        <button className="primary-button" onClick={() => dispatch({ type: "NAVIGATE", screen: "live" })}>Watch live <ChevronRight size={18} /></button>
      </section>
      {state.scenario === "help" && <button className="home-help-card" onClick={() => { dispatch({ type: "NAVIGATE", screen: "live" }); dispatch({ type: "OPEN_HELP" }); }}><span className="help-orb"><Sparkles size={19} /></span><span><strong>Aokie needs a quick answer</strong><small>Urgent drop-off after 5:00 pm?</small></span><ChevronRight size={19} /></button>}
      <section className="home-recent"><div className="section-title-line"><h2>Recent calls</h2><button onClick={() => dispatch({ type: "NAVIGATE", screen: "calls" })}>View all</button></div>{callHistory.slice(1,3).map((call) => <button className="compact-call-row" key={call.id} onClick={() => dispatch({ type: "OPEN_CALL_DETAIL", callId: call.id })}><span className={cn("result-icon", `is-${call.tone}`)}><Phone size={16} /></span><span><strong>{call.caller}</strong><small>{call.outcome}</small></span><span className="row-time">{call.time.split(" · ")[0]}<ChevronRight size={16} /></span></button>)}</section>
    </div>
  );
}

function LiveCallScreen({ state, dispatch }: { state: PrototypeState; dispatch: AppDispatch }) {
  const noControls = state.serviceMode === "unreachable" || state.scenario === "revoked" || state.callEnded;
  return (
    <div className={cn("screen live-screen", state.serviceMode === "human_active" && "is-human-live")}>
      <header className="live-topbar">
        <button className="icon-button" aria-label="Back to home" onClick={() => dispatch({ type: "NAVIGATE", screen: "home" })}><ArrowLeft size={20} /></button>
        <div><span>{business.name}</span><small>{business.line}</small></div><div className="live-time"><span className="fresh-dot" /> {state.callEnded ? "Ended" : currentCall.duration}</div>
        <button className="icon-button" aria-label="More call actions" onClick={() => dispatch({ type: "TOGGLE_MORE" })}><Ellipsis size={21} /></button>
        {state.moreOpen && <div className="more-menu"><button><Users size={16} /> Participants</button><button><Info size={16} /> Call details</button>{state.serviceMode === "human_active" && <button className="danger-link" onClick={() => dispatch({ type: "OPEN_END_CALL" })}><PhoneOff size={16} /> End caller call</button>}</div>}
      </header>
      <SafetyBanner state={state} />
      <section className="caller-card"><div className="caller-card-main"><span className="caller-avatar is-small">MT</span><div><h2>{currentCall.callerName}</h2><p>{currentCall.maskedNumber} · Existing customer</p></div><button aria-label="View caller record"><ChevronRight size={18} /></button></div><div className="trust-row"><span><ShieldCheck size={14} /> Disclosure verified</span><span><LockKeyhole size={14} /> Encrypted to this device</span></div></section>
      {state.scenario === "second-caller" && <aside className="callback-alert"><span><PhoneIncoming size={18} /></span><div><strong>Another caller was missed</strong><p>Identity verified · Callback queued for when this line is free.</p></div><BadgeCheck size={18} /></aside>}
      {state.scenario === "revoked" && <aside className="permission-alert"><AlertTriangle size={20} /><div><strong>Live permissions were revoked</strong><p>Audio access stopped immediately. Captions remain read-only.</p></div></aside>}
      {state.serviceMode === "unreachable" ? <UnknownStatePanel /> : state.serviceMode === "recovering" ? <RecoveryPanel countdown={state.recoveryCountdown} /> : state.serviceMode === "human_pending" ? <TransitionStepper step={state.pendingStep} /> : state.callEnded ? <CallEndedPanel dispatch={dispatch} /> : <><TranscriptFeed state={state} /><ParticipantStrip state={state} /></>}
      {!noControls && <CallControlDock state={state} dispatch={dispatch} />}
      {state.helpOpen && <HelpRequestSheet state={state} dispatch={dispatch} />}
      {state.takeoverOpen && <TakeoverDialog state={state} dispatch={dispatch} />}
      {state.endCallOpen && <EndCallerDialog dispatch={dispatch} />}
    </div>
  );
}

function SafetyBanner({ state }: { state: PrototypeState }) {
  let tone = "aokie"; let Icon: LucideIcon = Bot; let title = "AOKIE IS HANDLING THIS CALL"; let detail = "Microphone not requested · captions only";
  if (state.viewerMode === "monitor") { tone = "listening"; Icon = Headphones; title = "LISTENING — YOU CANNOT BE HEARD"; detail = "Receive-only audio · microphone technically unavailable"; }
  if (state.serviceMode === "human_pending") { tone = "hold"; Icon = ShieldCheck; title = "CONNECTING — YOU ARE NOT LIVE"; detail = "Caller safely held · your audio remains blocked"; }
  if (state.serviceMode === "human_active") { tone = "live"; Icon = state.micMuted ? MicOff : Mic; title = state.micMuted ? "LIVE CALL · MICROPHONE MUTED" : "YOU ARE LIVE TO THE CALLER"; detail = state.micMuted ? "The caller cannot hear you until you unmute" : "Desktop route confirmed · microphone live"; }
  if (state.serviceMode === "returning_to_aokie") { tone = "hold"; Icon = RefreshCw; title = "CALLER ON HOLD · RETURNING TO AOKIE"; detail = "Your microphone has been revoked"; }
  if (state.serviceMode === "recovering") { tone = "hold"; Icon = WifiOff; title = "CONNECTION LOST · CALLER SAFELY HELD"; detail = "Old audio lease revoked · secure recovery in progress"; }
  if (state.serviceMode === "unreachable") { tone = "unknown"; Icon = AlertTriangle; title = "CALL STATE UNCONFIRMED"; detail = "Controls unavailable until Desktop reconnects"; }
  if (state.scenario === "revoked") { tone = "unknown"; Icon = ShieldCheck; title = "READ-ONLY · LIVE ACCESS REVOKED"; detail = "No audio permission · authoritative state refreshed"; }
  if (state.callEnded) { tone = "ended"; Icon = PhoneOff; title = "CALL ENDED"; detail = "Audio off · record and follow-up saved"; }
  return <div className={cn("safety-banner", `is-${tone}`)} role="status" aria-live="assertive"><span className="safety-icon"><Icon size={21} /></span><div><strong>{title}</strong><small>{detail}</small></div>{tone === "aokie" && <AokieWave compact />}</div>;
}

function TranscriptFeed({ state }: { state: PrototypeState }) {
  const entries = useMemo(() => {
    if (state.serviceMode === "human_active") return [...transcript, { id: "human", speaker: "You", time: "04:18", text: "Hi Mia, Lance here. I can arrange the secure key drop for you." }];
    return transcript;
  }, [state.serviceMode]);
  return <section className="transcript-panel" aria-label="Live captions"><div className="transcript-heading"><div><span className="section-kicker">LIVE CAPTIONS</span><h2>Conversation</h2></div><span className="caption-fresh"><span /> Live · just now</span></div><div className="caption-list" aria-live="polite">{entries.map((entry) => { const isAokie = entry.speaker.startsWith("Aokie"); const isHuman = entry.speaker === "You"; return <article key={entry.id} className={cn("caption-row", isAokie && "is-aokie", isHuman && "is-human")}><span className="speaker-avatar">{isAokie ? <AokieMark compact /> : isHuman ? <UserCheck size={17} /> : <UserRound size={17} />}</span><div><div className="caption-meta"><strong>{entry.speaker}</strong><time>{entry.time}</time></div><p>{entry.text}</p></div></article>; })}{state.serviceMode === "aokie_active" && <div className="caption-composing"><AokieMark compact /><span>Aokie is listening</span><i /><i /><i /></div>}</div></section>;
}

function ParticipantStrip({ state }: { state: PrototypeState }) {
  return <button className="participant-strip" aria-label="View participants"><span className="avatar-stack"><i>PS</i><i>JL</i>{state.serviceMode === "human_active" && <i className="is-you">LB</i>}</span><span><strong>{state.serviceMode === "human_active" ? "You are speaking · 2 listening" : "2 people observing"}</strong><small>Priya Shah · Jordan Lee</small></span><ChevronRight size={18} /></button>;
}

function CallControlDock({ state, dispatch }: { state: PrototypeState; dispatch: AppDispatch }) {
  if (state.serviceMode === "human_pending") return <section className="call-control-dock transition-dock"><ShieldCheck size={17} /><span>Your microphone stays blocked until Desktop confirms the physical route.</span></section>;
  if (state.serviceMode === "human_active") return <section className="call-control-dock is-live"><div className="control-grid two-small"><button className={cn(state.micMuted && "is-selected")} onClick={() => dispatch({ type: "TOGGLE_MUTE" })}>{state.micMuted ? <MicOff /> : <Mic />}<span>{state.micMuted ? "Unmute" : "Mute"}</span></button><button onClick={() => dispatch({ type: "SET_AUDIO_ROUTE", route: state.audioRoute === "speaker" ? "earpiece" : "speaker" })}><Volume2 /><span>{state.audioRoute === "speaker" ? "Speaker" : "Earpiece"}</span></button></div><button className="return-button" onClick={() => dispatch({ type: "RETURN_TO_AOKIE" })}><Bot size={19} /> Return to Aokie</button><p><Info size={13} /> This leaves your Companion leg; it does not hang up Mia.</p></section>;
  return <section className="call-control-dock"><div className="control-grid three"><button className={cn(state.viewerMode === "monitor" && "is-selected")} onClick={() => dispatch({ type: "SET_SCENARIO", scenario: state.viewerMode === "monitor" ? "aokie" : "listening" })}><Headphones /><span>{state.viewerMode === "monitor" ? "Stop listening" : "Listen only"}</span></button><button className="is-help" onClick={() => dispatch({ type: "OPEN_HELP" })}><Sparkles /><span>Help Aokie</span></button><button className="is-takeover" onClick={() => dispatch({ type: "OPEN_TAKEOVER" })}><Mic /><span>Take over</span></button></div><p><MicOff size={13} /> Microphone not requested while watching or listening</p></section>;
}

function HelpRequestSheet({ state, dispatch }: { state: PrototypeState; dispatch: AppDispatch }) {
  return <Modal onClose={() => dispatch({ type: "CLOSE_HELP" })} label="Aokie help request"><div className="sheet-handle" /><div className="sheet-title-row"><span className="help-orb"><Sparkles size={20} /></span><div><span className="section-kicker">HELP REQUEST · URGENT</span><h2>Aokie needs your answer</h2></div><button className="icon-button" aria-label="Close" onClick={() => dispatch({ type: "CLOSE_HELP" })}><X size={19} /></button></div><div className="question-card"><MessageSquare size={18} /><div><strong>Can we accept an urgent drop-off after 5:00 pm today?</strong><p>Existing customer · 2019 Mazda 3 · no current booking</p></div><span className="countdown">00:42</span></div>{state.helpSent ? <div className="answer-success"><CheckCircle2 size={28} /><div><strong>Answer delivered privately</strong><p>Aokie will now explain the secure key drop to Mia.</p></div><button className="primary-button" onClick={() => dispatch({ type: "CLOSE_HELP" })}>Return to call</button></div> : <><label className="answer-field"><span>Your answer to Aokie</span><textarea value={state.helpAnswer} onChange={(event) => dispatch({ type: "SET_HELP_ANSWER", value: event.target.value })} rows={3} /></label><button className="primary-button" disabled={!state.helpAnswer.trim()} onClick={() => dispatch({ type: "SEND_HELP" })}><Send size={17} /> Send privately to Aokie</button><p className="permission-note"><ShieldCheck size={14} /> This text response does not open a microphone route.</p></>}</Modal>;
}

function TakeoverDialog({ state, dispatch }: { state: PrototypeState; dispatch: AppDispatch }) {
  const [progress, setProgress] = useState(0); const timerRef = useRef<number | null>(null); const completedRef = useRef(false);
  const stopTimer = () => { if (timerRef.current) window.clearInterval(timerRef.current); timerRef.current = null; };
  const cancelHold = () => { stopTimer(); if (!completedRef.current) setProgress(0); };
  const startHold = (event: ReactPointerEvent<HTMLButtonElement>) => { event.currentTarget.setPointerCapture(event.pointerId); completedRef.current = false; const started = performance.now(); stopTimer(); timerRef.current = window.setInterval(() => { const next = Math.min(100, ((performance.now() - started) / 1000) * 100); setProgress(next); if (next >= 100) { completedRef.current = true; stopTimer(); dispatch({ type: "START_TAKEOVER" }); } }, 24); };
  useEffect(() => () => stopTimer(), []);
  return <Modal onClose={() => dispatch({ type: "CLOSE_TAKEOVER" })} label="Confirm takeover"><div className="sheet-handle" /><div className="takeover-hero-icon"><Mic size={24} /></div><h2>Take over in Aokie?</h2><p className="modal-lead">Mia will hear a brief hold message while your secure audio connects. You will not be live until Desktop confirms the route.</p><div className="takeover-safety-list"><span><Check size={16} /> Caller placed on safe software hold</span><span><Check size={16} /> Exactly one person can speak</span><span><Check size={16} /> System end button returns to Aokie</span></div><button className="hold-to-confirm" style={{ "--hold-progress": `${progress}%` } as CSSProperties} onPointerDown={startHold} onPointerUp={cancelHold} onPointerCancel={cancelHold} onPointerLeave={cancelHold} onClick={() => !state.takeoverConfirmed && dispatch({ type: "ARM_TAKEOVER" })}><span className="hold-fill" /><Mic size={18} /><span>{state.takeoverConfirmed ? "Press confirm below" : "Press and hold to take over"}</span></button><div className="accessible-confirm"><span>Keyboard or screen reader?</span>{!state.takeoverConfirmed ? <button onClick={() => dispatch({ type: "ARM_TAKEOVER" })}>Use two-step confirmation</button> : <button className="confirm-link" onClick={() => dispatch({ type: "START_TAKEOVER" })}>Confirm secure takeover</button>}</div><button className="text-button" onClick={() => dispatch({ type: "CLOSE_TAKEOVER" })}>Stay in watch mode</button></Modal>;
}

function TransitionStepper({ step }: { step: number }) {
  const steps = [{ title: "Call claimed", detail: "First valid responder wins" },{ title: "Caller safely held", detail: "Aokie’s old audio route is flushed" },{ title: "Secure media ready", detail: "Microphone attached but still quarantined" },{ title: "Desktop route confirmed", detail: "Only then will you become live" }];
  return <section className="transition-panel"><div className="transition-orb"><ShieldCheck size={28} /></div><span className="section-kicker">SECURE TAKEOVER</span><h2>Connecting you to Mia</h2><p>The caller is hearing a short hold message. Your microphone is blocked.</p><div className="stepper-list">{steps.map((item,index) => { const number = index + 1; const done = number < step || step >= 4; const active = number === step; return <div key={item.title} className={cn("stepper-row", done && "is-done", active && "is-active")}><span>{done ? <Check size={15} /> : active ? <RefreshCw size={15} className="spin" /> : number}</span><div><strong>{item.title}</strong><small>{item.detail}</small></div></div>; })}</div></section>;
}

function RecoveryPanel({ countdown }: { countdown: number }) {
  return <section className="recovery-panel"><div className="recovery-ring" style={{ "--countdown": countdown } as CSSProperties}><span>{countdown}</span></div><span className="section-kicker">AUTOMATIC RECOVERY</span><h2>Reconnecting your secure audio</h2><p>The old talk lease was revoked immediately. Mia is safely hearing Aokie’s hold message.</p><div className="recovery-status"><span className="is-done"><Check size={15} /> Microphone blocked</span><span className="is-active"><RefreshCw size={15} className="spin" /> Fresh device admission</span><span>Fallback to Aokie if reconnect fails</span></div><small>Recovery never reuses delayed audio from the old connection.</small></section>;
}

function UnknownStatePanel() {
  return <section className="unknown-panel"><WifiOff size={34} /><h2>Desktop is unreachable</h2><p>The local phone call may still be continuing, so all remote controls are disabled. Companion will reconcile when the endpoint returns.</p><button className="secondary-button"><RefreshCw size={16} /> Retry secure status</button><div className="unknown-diagnostics"><span><Monitor size={15} /> Front Desk PC</span><strong>Last seen 8 sec ago</strong></div></section>;
}

function CallEndedPanel({ dispatch }: { dispatch: AppDispatch }) {
  return <section className="ended-panel"><span className="ended-icon"><Check size={25} /></span><h2>Call complete</h2><p>Aokie saved the transcript, outcome and follow-up to FormLogic.</p><div className="outcome-summary"><strong>Outcome</strong><span>After-hours key drop arranged</span><strong>Follow-up</strong><span>Inspection task · Tomorrow 8:00 am</span></div><button className="primary-button" onClick={() => dispatch({ type: "OPEN_CALL_DETAIL", callId: currentCall.id })}>View call record <ChevronRight size={17} /></button></section>;
}

function EndCallerDialog({ dispatch }: { dispatch: AppDispatch }) {
  return <Modal onClose={() => dispatch({ type: "CLOSE_END_CALL" })} label="End caller call" centered><div className="danger-modal-icon"><PhoneOff size={23} /></div><h2>End Mia’s cellular call?</h2><p className="modal-lead"><strong>This hangs up the caller’s cellular call.</strong> It is different from Return to Aokie, which keeps Mia connected.</p><button className="danger-button" onClick={() => dispatch({ type: "END_CALL" })}>End caller call</button><button className="secondary-button" onClick={() => dispatch({ type: "CLOSE_END_CALL" })}>Keep call connected</button></Modal>;
}

function CallsScreen({ state, dispatch }: { state: PrototypeState; dispatch: AppDispatch }) {
  const filtered = state.historyFilter === "All" ? callHistory : callHistory.filter((call) => call.category === state.historyFilter);
  return <div className="screen calls-screen"><AppHeader eyebrow="FORMLOGIC RECORDS" title="Calls" action={<button className="icon-button"><ListFilter size={19} /></button>} /><div className="history-summary"><div><strong>18</strong><span>Calls today</span></div><div><strong>14</strong><span>Aokie handled</span></div><div><strong>2</strong><span>Need follow-up</span></div></div><div className="filter-scroller" role="group" aria-label="Filter call history">{filterOptions.map((filter) => <button key={filter} className={state.historyFilter === filter ? "is-active" : ""} onClick={() => dispatch({ type: "SET_HISTORY_FILTER", filter })}>{filter}</button>)}</div><section className="call-history-list"><div className="section-title-line"><h2>{state.historyFilter === "All" ? "Today" : state.historyFilter}</h2><span>{filtered.length} records</span></div>{filtered.map((call) => <button className="history-card" key={call.id} onClick={() => dispatch({ type: "OPEN_CALL_DETAIL", callId: call.id })}><span className={cn("result-icon", `is-${call.tone}`)}><Phone size={17} /></span><span className="history-card-copy"><strong>{call.caller}</strong><small>{call.number} · {call.time}</small><em className={cn(`is-${call.tone}`)}>{call.outcome}</em></span><ChevronRight size={18} /></button>)}{filtered.length === 0 && <div className="empty-state"><History size={28} /><strong>No calls in this view</strong><p>Choose another filter to see recent records.</p></div>}</section></div>;
}

function CallDetailScreen({ state, dispatch }: { state: PrototypeState; dispatch: AppDispatch }) {
  const call = callHistory.find((item) => item.id === state.selectedCallId) ?? callHistory[1];
  return <div className="screen detail-screen"><header className="detail-header"><button className="icon-button" onClick={() => dispatch({ type: "NAVIGATE", screen: "calls" })}><ArrowLeft size={20} /></button><div><span className="section-kicker">CALL RECORD</span><h1>{call.caller}</h1></div><button className="icon-button"><Ellipsis size={20} /></button></header><section className="detail-hero"><span className={cn("result-icon large", `is-${call.tone}`)}><Phone size={22} /></span><h2>{call.outcome}</h2><p>{call.time} · {call.number}</p><div className="detail-tags"><span><BadgeCheck size={13} /> Disclosure verified</span><span><Database size={13} /> Saved to FormLogic</span></div></section><section className="detail-section"><span className="section-kicker">SUMMARY</span><p className="summary-copy">{call.summary}</p></section><section className="detail-section"><div className="section-title-line"><h2>Activity</h2><span>Audited</span></div><div className="timeline"><TimelineItem icon={PhoneIncoming} title="Call received" detail="Aokie answered after 2 rings" time="0:00" /><TimelineItem icon={Bot} title="Conversation handled" detail="Intent and caller details captured" time="0:12" /><TimelineItem icon={Sparkles} title="Team help requested" detail="Priya replied privately" time="3:06" /><TimelineItem icon={CheckCircle2} title="Follow-up created" detail="Inspection task for tomorrow" time="6:18" /></div></section><section className="detail-section"><div className="section-title-line"><h2>Transcript</h2><button>Open full</button></div>{transcript.slice(0,3).map((item) => <div className="mini-transcript" key={item.id}><strong>{item.speaker}</strong><p>{item.text}</p></div>)}</section></div>;
}

function TimelineItem({ icon: Icon, title, detail, time }: { icon: LucideIcon; title: string; detail: string; time: string }) {
  return <div className="timeline-item"><span><Icon size={15} /></span><div><strong>{title}</strong><small>{detail}</small></div><time>{time}</time></div>;
}

function TeamScreen() {
  const [members, setMembers] = useState(teamMembers);
  const move = (index: number, direction: -1 | 1) => { const nextIndex = index + direction; if (nextIndex < 0 || nextIndex >= members.length) return; const next = [...members]; [next[index], next[nextIndex]] = [next[nextIndex], next[index]]; setMembers(next); };
  const toggleAlert = (id: string) => setMembers((current) => current.map((member) => member.id === id ? { ...member, alert: !member.alert } : member));
  return <div className="screen team-screen"><AppHeader eyebrow="PRIMARY ROUTING GROUP" title="Team & routing" action={<button className="icon-button"><UserCheck size={19} /></button>} /><section className="routing-overview"><div><span className="routing-icon"><Network size={20} /></span><span><strong>Priority escalation</strong><small>Alert in order · 20 sec between people</small></span></div><button>Configure</button></section><div className="coverage-banner"><span><Users size={18} /></span><div><strong>Good coverage until 6:30 pm</strong><p>3 team members can answer live offers.</p></div><CheckCircle2 size={19} /></div><section className="routing-list"><div className="section-title-line"><h2>Escalation order</h2><span>Drag or use arrows</span></div>{members.map((member,index) => <article className="member-card" key={member.id}><GripVertical className="drag-handle" size={18} /><span className={cn("member-avatar", member.status.startsWith("Unavailable") && "is-offline")}>{member.initials}</span><div className="member-copy"><strong>{member.name}{member.id === "lance" && <em>You</em>}</strong><small>{member.role} · {member.status}</small><span>{member.group}</span></div><div className="member-controls"><button aria-label={`Move ${member.name} up`} disabled={index === 0} onClick={() => move(index,-1)}><ArrowUp size={14} /></button><button aria-label={`Move ${member.name} down`} disabled={index === members.length - 1} onClick={() => move(index,1)}><ArrowDown size={14} /></button><button className={cn("mini-toggle", member.alert && "is-on")} aria-label={`${member.alert ? "Disable" : "Enable"} alerts for ${member.name}`} onClick={() => toggleAlert(member.id)}><span /></button></div></article>)}</section><aside className="web-permissions-note"><ShieldCheck size={18} /><div><strong>Roles stay in FormLogic</strong><p>Owners edit security grants in the FormLogic web app, not on mobile.</p></div><ExternalLink size={16} /></aside></div>;
}

function SettingsScreen({ state, dispatch }: { state: PrototypeState; dispatch: AppDispatch }) {
  return <div className="screen settings-screen"><AppHeader eyebrow="COASTAL AUTO CARE" title="Settings" /><section className="account-card"><span className="member-avatar">LB</span><div><strong>Lance Baker</strong><small>Owner · {business.deployment}</small></div><ChevronRight size={18} /></section><section className="settings-section"><span className="section-kicker">CONNECTION</span><div className="deployment-switch"><button className={state.deploymentMode === "hosted" ? "is-active" : ""} onClick={() => dispatch({ type: "SET_DEPLOYMENT", mode: "hosted" })}><Cloud size={17} /> Managed</button><button className={state.deploymentMode === "self-hosted" ? "is-active" : ""} onClick={() => dispatch({ type: "SET_DEPLOYMENT", mode: "self-hosted" })}><Server size={17} /> Self-hosted</button></div>{healthItems.map((item,index) => <div className="settings-row" key={item.label}><span className="settings-row-icon">{index === 0 ? <Monitor /> : index === 1 ? <Smartphone /> : index === 2 ? <Network /> : index === 3 ? <Router /> : <Bell />}</span><span><strong>{item.label}</strong><small>{item.detail}</small></span><em>{item.value}<CheckCircle2 size={14} /></em></div>)}</section><section className="settings-section"><span className="section-kicker">PRIVACY & AUDIO</span><div className="settings-row is-toggle"><span className="settings-row-icon"><ShieldCheck /></span><span><strong>Monitoring disclosure</strong><small>{state.disclosureEnabled ? "Required announcement verified" : "Listening is blocked"}</small></span><button className={cn("mini-toggle", state.disclosureEnabled && "is-on")} onClick={() => dispatch({ type: "TOGGLE_DISCLOSURE" })}><span /></button></div><div className="settings-row"><span className="settings-row-icon"><Mic /></span><span><strong>Microphone access</strong><small>Ask only when taking over a caller</small></span><em className="neutral">Ask when needed</em></div><div className="settings-row"><span className="settings-row-icon"><LockKeyhole /></span><span><strong>Secure media</strong><small>Desktop ↔ this enrolled device</small></span><em>Verified<BadgeCheck size={14} /></em></div></section><section className="settings-section"><span className="section-kicker">THIS DEVICE</span><div className="settings-row"><span className="settings-row-icon"><Smartphone /></span><span><strong>Lance’s iPhone</strong><small>Approved · Key …A41F</small></span><em>Active<CheckCircle2 size={14} /></em></div><button className="settings-action" onClick={() => dispatch({ type: "NAVIGATE", screen: "onboarding" })}><ScanLine size={17} /><span><strong>Run setup checks again</strong><small>Pairing, notifications and audio</small></span><ChevronRight size={18} /></button></section></div>;
}

function OnboardingScreen({ state, dispatch }: { state: PrototypeState; dispatch: AppDispatch }) {
  const step = state.onboardingStep;
  return <div className="screen onboarding-screen"><header className="onboarding-header">{step > 0 ? <button className="icon-button" onClick={() => dispatch({ type: "SET_ONBOARDING_STEP", step: step - 1 })}><ArrowLeft size={20} /></button> : <span />}<div className="onboarding-progress" aria-label={`Step ${step + 1} of 4`}>{[0,1,2,3].map((item) => <i key={item} className={item <= step ? "is-active" : ""} />)}</div><button className="text-button" onClick={() => dispatch({ type: "NAVIGATE", screen: "home" })}>Skip demo</button></header>
  {step === 0 && <section className="onboarding-content welcome-step"><div className="welcome-mark"><AokieMark /></div><span className="section-kicker">AOKIE COMPANION</span><h1>Your front desk, wherever you are.</h1><p>Watch Aokie handle calls, answer private questions, and securely take over when a caller needs you.</p><div className="welcome-points"><span><Eye size={18} /><strong>Watch without joining audio</strong></span><span><Headphones size={18} /><strong>Listen without being heard</strong></span><span><ShieldCheck size={18} /><strong>Take over through a secure app bridge</strong></span></div><aside className="same-phone-warning is-supported"><ShieldCheck size={19} /><div><strong>Companion is the microphone and speaker endpoint</strong><p>It never connects to the Bluetooth dongle directly. Aokie Desktop bridges caller audio to this enrolled device through WebRTC; the web or custom server carries secure signalling. Only the exact phone also carrying the cellular call has an HFP/VoIP route conflict.</p></div></aside><button className="primary-button" onClick={() => dispatch({ type: "SET_ONBOARDING_STEP", step: 1 })}>Get started <ChevronRight size={18} /></button></section>}
  {step === 1 && <section className="onboarding-content"><span className="section-kicker">STEP 1 OF 3</span><h1>Connect your deployment</h1><p>Sign in to the managed service or scan the signed setup QR from your own server.</p><button className={cn("deployment-card", state.deploymentMode === "hosted" && "is-selected")} onClick={() => dispatch({ type: "SET_DEPLOYMENT", mode: "hosted" })}><span><Cloud size={23} /></span><div><strong>Managed Aokie</strong><p>Sign in with your FormLogic account</p></div>{state.deploymentMode === "hosted" ? <CheckCircle2 size={20} /> : <ChevronRight size={20} />}</button><button className={cn("deployment-card", state.deploymentMode === "self-hosted" && "is-selected")} onClick={() => dispatch({ type: "SET_DEPLOYMENT", mode: "self-hosted" })}><span><ScanLine size={23} /></span><div><strong>Self-hosted server</strong><p>Scan discovery and trust details</p></div>{state.deploymentMode === "self-hosted" ? <CheckCircle2 size={20} /> : <ChevronRight size={20} />}</button><div className="workspace-preview"><span className="workspace-logo">CA</span><div><strong>Coastal Auto Care</strong><p>Active membership · Owner</p></div><BadgeCheck size={19} /></div><button className="primary-button" onClick={() => dispatch({ type: "SET_ONBOARDING_STEP", step: 2 })}>Continue securely <ChevronRight size={18} /></button></section>}
  {step === 2 && <section className="onboarding-content security-step"><span className="section-kicker">STEP 2 OF 3 · INTERACTIVE DEMO</span><h1>Approve this device</h1><p>This screen illustrates the checks a live native enrollment must complete. Demo values below are not trusted credentials.</p><div className="pairing-visual"><span className="paired-device"><Smartphone size={28} /><small>This endpoint</small></span><span className="pairing-line"><i /><LockKeyhole size={18} /><i /></span><span className="paired-device"><Monitor size={28} /><small>Front Desk PC</small></span></div><div className="fingerprint-card"><KeyRound size={19} /><div><strong>Example device identity</strong><p>Illustrative thumbprint · not enrolled</p></div><BadgeCheck size={19} /></div><div className="checklist"><CheckItem icon={ShieldCheck} title="Discovery trust" detail="Live native check required" /><CheckItem icon={LockKeyhole} title="Desktop key approval" detail="Owner confirmation required" /><CheckItem icon={Router} title="Secure media path" detail="Desktop ↔ Companion WebRTC · TURN fallback" /><CheckItem icon={Smartphone} title="Independent audio endpoint" detail="Uses this device's mic and speakers; never the dongle" /></div><button className="primary-button" onClick={() => dispatch({ type: "SET_ONBOARDING_STEP", step: 3 })}>Continue demo <ChevronRight size={18} /></button></section>}
  {step === 3 && <section className="onboarding-content permissions-step"><span className="section-kicker">STEP 3 OF 3</span><h1>Choose how Aokie can reach you</h1><p>Notifications and microphone access are separate. Watching or listening never needs your microphone.</p><div className="permission-card"><span><Bell size={22} /></span><div><strong>Notifications</strong><p>Call alerts and genuine live voice offers</p></div><em>Allowed<CheckCircle2 size={15} /></em></div><div className="permission-card"><span><Mic size={22} /></span><div><strong>Microphone</strong><p>Ask only when taking over a caller</p></div><em className="neutral">Not requested</em></div><div className="permission-card"><span><Eye size={22} /></span><div><strong>Listen-only safety</strong><p>Receive-only media; microphone unavailable</p></div><em>Enforced<ShieldCheck size={15} /></em></div><aside className="privacy-explainer"><LockKeyhole size={18} /><div><strong>Accurate encryption promise</strong><p>Call media is encrypted between Aokie Desktop and this enrolled device. The carrier and Bluetooth legs remain outside that boundary.</p></div></aside><button className="primary-button" onClick={() => dispatch({ type: "NAVIGATE", screen: "home" })}><Check size={18} /> Finish demo setup</button></section>}</div>;
}

function CheckItem({ icon: Icon, title, detail }: { icon: LucideIcon; title: string; detail: string }) {
  return <div className="check-item"><span><Icon size={17} /></span><div><strong>{title}</strong><small>{detail}</small></div><CheckCircle2 size={18} /></div>;
}

function PersistentCallCapsule({ dispatch }: { dispatch: AppDispatch }) {
  return <button className="persistent-call" onClick={() => dispatch({ type: "NAVIGATE", screen: "live" })}><span className="pulse-dot" /><span><strong>Mia Thompson</strong><small>Aokie is handling · 04:18</small></span><AokieWave compact /><ChevronRight size={17} /></button>;
}

function MobileNav({ screen, dispatch }: { screen: Screen; dispatch: AppDispatch }) {
  return <nav className="mobile-nav" aria-label="Main navigation">{navItems.map(({ id,label,icon: Icon }) => <button key={id} className={(screen === id || (screen === "call-detail" && id === "calls")) ? "is-active" : ""} aria-current={screen === id ? "page" : undefined} onClick={() => dispatch({ type: "NAVIGATE", screen: id })}><Icon size={21} /><span>{label}</span></button>)}</nav>;
}

function ScenarioPanel({ state, dispatch }: { state: PrototypeState; dispatch: AppDispatch }) {
  const callFlow = scenarios.filter((item) => item.group === "Call flow"); const failures = scenarios.filter((item) => item.group === "Safety & failure");
  const selectScenario = (scenario: Scenario) => { dispatch({ type: "SET_SCENARIO", scenario }); dispatch({ type: "NAVIGATE", screen: "live" }); };
  return <aside className="scenario-panel" aria-label="Prototype controls"><div className="scenario-header"><div><span className="section-kicker">INTERACTIVE DEMO</span><h2>Prototype scenarios</h2></div><Sparkles size={19} /></div><p className="scenario-intro">Jump between the authoritative call states described in the implementation guide.</p><ScenarioGroup title="Call flow" items={callFlow} state={state} onSelect={selectScenario} /><ScenarioGroup title="Safety & failure" items={failures} state={state} onSelect={selectScenario} /><div className="scenario-divider" /><span className="scenario-label">APP SCREENS</span><div className="screen-shortcuts">{(["home","calls","team","settings","onboarding"] as Screen[]).map((screen) => <button key={screen} className={state.screen === screen ? "is-active" : ""} onClick={() => dispatch({ type: "NAVIGATE", screen })}>{screen === "onboarding" ? "Setup" : screen[0].toUpperCase() + screen.slice(1)}</button>)}</div><div className="prototype-facts"><span><ShieldCheck size={15} /> Reducer-backed state</span><span><Gauge size={15} /> Deterministic transitions</span><span><Smartphone size={15} /> 390px mobile target</span></div><button className="reset-button" onClick={() => dispatch({ type: "RESET" })}><RotateCcw size={16} /> Reset prototype</button><p className="scenario-footnote">Demo data only · no real call or microphone access.</p></aside>;
}

function ScenarioGroup({ title, items, state, onSelect }: { title: string; items: typeof scenarios; state: PrototypeState; onSelect: (scenario: Scenario) => void }) {
  const icons: Record<Scenario, LucideIcon> = { aokie: Bot, listening: Headphones, help: HelpCircle, pending: ShieldCheck, human: Mic, recovery: RefreshCw, "second-caller": PhoneIncoming, offline: WifiOff, revoked: ShieldCheck, ended: PhoneOff };
  return <div className="scenario-group"><span className="scenario-label">{title}</span>{items.map((item) => { const Icon = icons[item.id]; return <button key={item.id} className={state.scenario === item.id ? "is-active" : ""} onClick={() => onSelect(item.id)}><Icon size={17} /><span>{item.label}</span>{state.scenario === item.id ? <CheckCircle2 size={16} /> : <ChevronRight size={16} />}</button>; })}</div>;
}

function Modal({ children, onClose, label, centered = false }: { children: ReactNode; onClose: () => void; label: string; centered?: boolean }) {
  return <div className={cn("modal-layer", centered && "is-centered")} role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}><section className={cn("modal-sheet", centered && "is-dialog")} role="dialog" aria-modal="true" aria-label={label}>{children}</section></div>;
}

function AokieWave({ compact = false }: { compact?: boolean }) {
  return <span className={cn("aokie-wave", compact && "is-compact")} aria-label="Aokie audio active">{[9,16,24,13,28,18,10,21,14].map((height,index) => <i key={`${height}-${index}`} style={{ height, animationDelay: `${index * 70}ms` }} />)}</span>;
}

function Toast({ message }: { message: string }) {
  return <div className="toast" role="status"><CheckCircle2 size={17} /><span>{message}</span></div>;
}
