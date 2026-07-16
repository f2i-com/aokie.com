import type { Scenario } from "../../state/prototypeModel";

export const business = {
  name: "Coastal Auto Care",
  line: "Main reception",
  timezone: "Australia/Sydney",
  deployment: "Managed · Sydney",
  disclosure: "Active · Policy v2",
};

export const currentCall = {
  id: "CALL-2026-0715-0047",
  callerName: "Mia Thompson",
  maskedNumber: "+61 4•• ••• 782",
  intent: "Urgent brake warning-light enquiry",
  duration: "04:18",
  handlingMode: "Aokie",
};

export const transcript = [
  {
    id: "t1",
    speaker: "Aokie",
    time: "04:10",
    text: "Thanks for calling Coastal Auto Care. How can I help today?",
  },
  {
    id: "t2",
    speaker: "Caller",
    time: "04:12",
    text: "Hi, a brake warning light came on and I’m hoping I can drop the car in after work.",
  },
  {
    id: "t3",
    speaker: "Aokie",
    time: "04:14",
    text: "I can check that for you. What time could you arrive?",
  },
  {
    id: "t4",
    speaker: "Caller",
    time: "04:16",
    text: "Around five-thirty.",
  },
];

export const callHistory = [
  {
    id: "CALL-2026-0715-0047",
    caller: "Mia Thompson",
    number: "+61 4•• ••• 782",
    time: "Now · 4 min",
    outcome: "Aokie handling",
    category: "Aokie handled",
    tone: "active",
    summary: "Urgent brake warning-light enquiry; checking an after-hours key drop.",
  },
  {
    id: "CALL-2026-0715-0042",
    caller: "Ethan Williams",
    number: "+61 4•• ••• 291",
    time: "3:42 pm · 6 min",
    outcome: "Booking captured",
    category: "Aokie handled",
    tone: "success",
    summary: "Booked a logbook service request for Thursday morning.",
  },
  {
    id: "CALL-2026-0715-0039",
    caller: "Sofia Nguyen",
    number: "+61 4•• ••• 440",
    time: "2:18 pm · 9 min",
    outcome: "Taken over by Priya",
    category: "Taken over",
    tone: "private",
    summary: "Supervisor handled a warranty question and returned the record to Aokie.",
  },
  {
    id: "CALL-2026-0715-0035",
    caller: "Private caller",
    number: "Caller ID unavailable",
    time: "12:06 pm",
    outcome: "Callback queued",
    category: "Callback",
    tone: "hold",
    summary: "Missed while the line was busy; callback identity verified and queued.",
  },
  {
    id: "CALL-2026-0715-0028",
    caller: "Oliver Brown",
    number: "+61 4•• ••• 903",
    time: "10:22 am",
    outcome: "Callback needs attention",
    category: "Missed",
    tone: "danger",
    summary: "Automated callback failed. A team member needs to follow up.",
  },
];

export const teamMembers = [
  { id: "lance", initials: "LB", name: "Lance Baker", role: "Owner", status: "Available", group: "Primary", alert: true },
  { id: "priya", initials: "PS", name: "Priya Shah", role: "Supervisor", status: "Available", group: "Primary", alert: true },
  { id: "jordan", initials: "JL", name: "Jordan Lee", role: "Receptionist", status: "Quiet until 6:30 pm", group: "Primary", alert: true },
  { id: "mei", initials: "MC", name: "Mei Chen", role: "Advisor", status: "Help requests", group: "Escalation", alert: true },
  { id: "noah", initials: "NG", name: "Noah Green", role: "Observer", status: "Unavailable", group: "Observers", alert: false },
];

export const healthItems = [
  { label: "Front Desk PC", value: "Online", detail: "Last seen just now" },
  { label: "Reception phone", value: "Connected", detail: "HFP audio ready" },
  { label: "Realtime gateway", value: "Healthy", detail: "Sydney · 38 ms" },
  { label: "TURN relay", value: "Ready", detail: "Direct path preferred" },
  { label: "Push delivery", value: "Healthy", detail: "Tested 2 min ago" },
];

export const scenarios: Array<{ id: Scenario; label: string; group: "Call flow" | "Safety & failure" }> = [
  { id: "aokie", label: "Aokie active", group: "Call flow" },
  { id: "listening", label: "Listen only", group: "Call flow" },
  { id: "help", label: "Help request", group: "Call flow" },
  { id: "pending", label: "Takeover pending", group: "Call flow" },
  { id: "human", label: "You are live", group: "Call flow" },
  { id: "recovery", label: "Connection loss", group: "Safety & failure" },
  { id: "second-caller", label: "Second caller", group: "Safety & failure" },
  { id: "offline", label: "Gateway unavailable", group: "Safety & failure" },
  { id: "revoked", label: "Permission revoked", group: "Safety & failure" },
  { id: "ended", label: "Call ended", group: "Safety & failure" },
];
