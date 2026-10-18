import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

// Exercise the shipped plain-JS screen with an isolated host. No device,
// network, call-control or settings-write commands are issued by this check.
const base = new URL('../crates/aokie-plugin/ui/receptionist/', import.meta.url);
const elements = new Map();
const element = id => {
  if (!elements.has(id)) elements.set(id, {
    innerHTML: '', textContent: '', hidden: false, disabled: false,
    addEventListener() {}, setAttribute() {}, classList: { toggle() {} },
  });
  return elements.get(id);
};
const payloads = {
  'phone.status': { connected: true, paired: true, device: { name: 'Test phone' } },
  'phone.listPaired': { devices: [{ address: 'test-device', connected: true }] },
  'settings.get': { settings: { aiReceptionist: false } },
  'call.switchboard': { foreground: null, waiting: null, parked: null, revision: 0, switchInProgress: false, callHeldState: 0 },
};
let failed = false;
let snapshot = { state: 'running', health: { status: 'ok' } };
let oaiyStatus = { voice: { chosen: 'receptionist' }, llm: { running: true, state: 'ready', resident: 'engine-model' } };
let sources = [{ kind: 'service', serviceId: 'oaiy-voice', id: 'service:oaiy-voice', status: 'running', url: 'http://127.0.0.1:8783' }];
const host = {
  aiSources: async () => {
    if (failed) throw new Error('Host unavailable');
    return sources;
  },
  snapshot: async () => {
    if (failed) throw new Error('Host unavailable');
    return snapshot;
  },
  command: async command => {
    assert(Object.hasOwn(payloads, command), `Unexpected command ${command}`);
    if (failed) throw new Error('Host unavailable');
    return payloads[command];
  },
  events: { subscribe: async () => ({ unsubscribe() {} }) },
  toast() {},
  navigate: async () => true,
  oaiyStatus: async () => {
    if (failed) throw new Error('Host unavailable');
    return oaiyStatus;
  },
};
const window = { PluginHost: host, addEventListener() {}, setInterval() {}, clearInterval() {} };
const rootClasses = new Set();
const document = {
  hidden: true, getElementById: element, addEventListener() {}, querySelectorAll: () => [],
  documentElement: { classList: { add: (c) => rootClasses.add(c), remove: (c) => rootClasses.delete(c) } },
};
const context = vm.createContext({ window, document, URL, setTimeout, clearTimeout });
const appSource = await readFile(new URL('app.js', base), 'utf8');
vm.runInContext(appSource.replace(/\}\)\(\);\s*$/, `
  window.testApp = { state, refreshSnapshot, refreshPhoneStatus, refreshPhones, refreshSettings, refreshSources, refreshOaiyStatus, refreshSwitchboard, renderLive, replyOwner };
})();`), context);
const app = window.testApp;
app.state.call = { callId: 'fixture', state: 'active' };
await Promise.all([app.refreshSnapshot(), app.refreshPhoneStatus(), app.refreshPhones(), app.refreshSettings()]);
assert.equal(element('hero-headline').textContent, 'Ready for calls');
assert.match(element('readiness').innerHTML, /Bluetooth linked/);
// Operator mode: what is typed would be said in Aokie's own voice, which it
// does not have (OAIY Voice speaks on calls that go to OAIY).
assert.equal(element('speak-send').disabled, true);
assert.match(element('live-body').innerHTML, /no speech of its own/);
snapshot = { state: 'unhealthy', lastHealth: { status: 'degraded', detail: 'Phone is disconnected', components: { responder: { mode: 'agent', ready: true } } }, manifest: { version: '0.2.0' } };
await app.refreshSnapshot();
assert.equal(element('hero-headline').textContent, 'Running — needs attention');
assert.equal(element('hero-sub').textContent, 'Phone is disconnected');
// Off the realtime route nobody answers: Aokie has no speech of its own.
assert.match(element('readiness').innerHTML, /Nobody/);
assert.match(element('readiness').innerHTML, /send them to OAIY/);
assert.doesNotMatch(element('readiness').innerHTML, /LLM ready/);
assert.equal(element('hero-version').textContent, 'Aokie v0.2.0');
snapshot = { state: 'unhealthy', lastHealthError: 'Health probe timed out', health: { status: 'ok', components: { responder: { mode: 'agent', ready: true } } } };
await app.refreshSnapshot();
assert.equal(element('hero-headline').textContent, 'Running — needs attention');
assert.equal(element('hero-sub').textContent, 'Health probe timed out');
assert.equal(element('hero-pill-text').textContent, 'Needs attention');
assert.match(element('readiness').innerHTML, /Health probe timed out/);
assert.doesNotMatch(element('readiness').innerHTML, /LLM ready|plugin not running/);
snapshot = { state: 'unhealthy' };
await app.refreshSnapshot();
assert.equal(element('hero-headline').textContent, 'Running — needs attention');
assert.doesNotMatch(element('readiness').innerHTML, /LLM ready|plugin not running/);
snapshot = { state: 'running' };
await app.refreshSnapshot();
assert.equal(element('hero-headline').textContent, 'Starting up');
snapshot = { state: 'crashed', reason: 'Process exited' };
await app.refreshSnapshot();
assert.equal(element('hero-headline').textContent, 'Aokie is not running');
assert.equal(element('hero-sub').textContent, 'Process exited');
snapshot = { state: 'running', lastHealth: { status: 'ok' }, manifest: { version: '0.2.1' } };
await app.refreshSnapshot();
assert.equal(element('hero-headline').textContent, 'Ready for calls');
assert.equal(element('hero-version').textContent, 'Aokie v0.2.1');
failed = true;
await Promise.all([app.refreshSnapshot(), app.refreshPhoneStatus(), app.refreshPhones(), app.refreshSettings()]);
assert.equal(element('hero-headline').textContent, 'Plugin status unavailable');
assert.doesNotMatch(element('readiness').innerHTML, /Bluetooth linked/);
assert.equal(element('phones-title').textContent, 'Paired phones unavailable');
assert.equal(app.replyOwner(), 'unknown');
assert.equal(element('speak-send').disabled, true);
failed = false;
await Promise.all([app.refreshSnapshot(), app.refreshPhoneStatus(), app.refreshPhones(), app.refreshSettings()]);
assert.equal(element('hero-headline').textContent, 'Ready for calls');
assert.equal(element('speak-send').disabled, true);
for (const [value, owner] of [[true, 'agent'], ['true', 'agent'], [false, 'operator'], ['false', 'operator'], [undefined, 'unknown'], ['invalid', 'unknown']]) {
  app.state.settings = { settings: { aiReceptionist: value } };
  assert.equal(app.replyOwner(), owner);
}
for (const direction of ['inbound', 'outbound']) {
  app.state.call = { callId: 'fixture', state: 'ringing', direction };
  app.renderLive();
  assert.equal(element('live-pill-text').textContent, direction === 'outbound' ? 'Dialing' : 'Ringing');
  assert.equal(/data-act="call-answer"/.test(element('live-body').innerHTML), direction === 'inbound');
  assert.equal(/data-act="call-hangup"/.test(element('live-body').innerHTML), direction === 'outbound');
  assert.equal(element('speak-row').hidden, true);
  app.state.call.state = 'active';
  app.renderLive();
  assert.match(element('live-body').innerHTML, new RegExp(direction === 'outbound' ? 'OUTBOUND CALL' : 'INCOMING CALL'));
  assert.doesNotMatch(element('live-body').innerHTML, /data-act="call-answer"/);
}
await app.refreshSwitchboard();
assert.equal(element('switchboard-body').hidden, true);
payloads['call.switchboard'] = { waiting: { callId: 'waiting', from: '<img src=x>' }, parked: { callId: 'held', from: '' }, switchInProgress: true, revision: 2, callHeldState: 1 };
await app.refreshSwitchboard();
assert.equal(element('switchboard-body').hidden, false);
assert.match(element('switchboard-body').innerHTML, /Waiting caller/);
assert.match(element('switchboard-body').innerHTML, /On hold/);
assert.match(element('switchboard-body').innerHTML, /Unknown number/);
assert.match(element('switchboard-body').innerHTML, /Switching callers/);
assert.match(element('switchboard-body').innerHTML, /&lt;img src=x&gt;/);
assert.doesNotMatch(element('switchboard-body').innerHTML, /<button/);
failed = true;
await app.refreshSwitchboard();
assert.match(element('switchboard-body').innerHTML, /Call waiting status unavailable/);
assert.doesNotMatch(element('switchboard-body').innerHTML, /Waiting caller|On hold/);
failed = false;
payloads['call.switchboard'] = {};
await app.refreshSwitchboard();
assert.match(element('switchboard-body').innerHTML, /did not return a call waiting snapshot/);
payloads['call.switchboard'] = { waiting: null, parked: null, switchInProgress: false };
await app.refreshSwitchboard();
assert.equal(element('switchboard-body').hidden, true);

// Where calls go: OAIY's own gateway, another realtime provider, Aokie's
// older local speech, or no AI receptionist.
const oaiy = window.__aokieTabs.oaiy;
const oaiyBag = { aiReceptionist: true, realtimeVoiceMode: 'desktop_realtime', realtimeVoiceEndpoint: oaiy.endpoint, realtimeVoiceDestination: oaiy.destination };
assert.equal(oaiy.endpoint, 'ws://127.0.0.1:17872/api/ai/providers/oaiy/v1/realtime/stream');
assert.equal(oaiy.realtimeProviderId(oaiy.endpoint), 'oaiy');
assert.equal(oaiy.realtimeProviderId('http://127.0.0.1:17872/api/ai/providers/oaiy/v1/chat/completions'), '');
assert.equal(oaiy.callRoute(oaiyBag), 'oaiy');
assert.equal(oaiy.callRoute({ ...oaiyBag, realtimeVoiceEndpoint: 'ws://127.0.0.1:17872/api/ai/providers/openai/v1/realtime/stream' }), 'realtime');
assert.equal(oaiy.callRoute({ aiReceptionist: true, realtimeVoiceMode: 'legacy' }), 'local');
assert.equal(oaiy.callRoute({ aiReceptionist: 'false' }), 'flows');
assert.equal(oaiy.callRoute(null), 'unknown');
assert.equal(JSON.stringify(oaiy.voiceService(sources)), JSON.stringify({ status: 'running', url: 'http://127.0.0.1:8783' }));
assert.equal(oaiy.voiceService([]), null);

// The Overview names OAIY's Front desk, and OAIY Voice's live state, on the
// OAIY route — and never Aokie's own speech engine or model.
payloads['settings.get'] = { configVersion: 3, settings: { ...oaiyBag, greeting: 'Hi', persona: '', ttsEngine: '', aiModel: 'old-model' } };
snapshot = { state: 'running', lastHealth: { status: 'ok', components: { responder: { mode: 'desktop_realtime', ready: true }, radio: { voiceRuntime: { realtime: { error: null } } } } } };
await Promise.all([app.refreshSnapshot(), app.refreshSettings(), app.refreshSources()]);
assert.match(element('readiness').innerHTML, /OAIY Front desk/);
assert.match(element('readiness').innerHTML, /OAIY Voice running/);
assert.match(element('settings-body').innerHTML, /OAIY Front desk/);
assert.match(element('settings-body').innerHTML, /Chosen in Calendar/);
assert.doesNotMatch(element('settings-body').innerHTML, /Pocket|Sherpa|old-model|LLM/);
assert.equal(element('settings-title').textContent, 'Calls go to OAIY');
// The host's live values and links: the chosen voice and the engines' model,
// each a link to the dashboard page where it is set.
await app.refreshOaiyStatus();
assert.match(element('settings-body').innerHTML, /data-nav="calendar">receptionist</);
assert.match(element('settings-body').innerHTML, /data-nav="engines">engine-model · ready</);
assert.equal(rootClasses.has('rcp-no-nav'), false);
oaiyStatus = { voice: null, llm: { running: false, state: null, resident: null } };
await app.refreshOaiyStatus();
assert.match(element('settings-body').innerHTML, /Chosen in Calendar/);
assert.match(element('settings-body').innerHTML, /engines stopped/);
failed = true;
await app.refreshOaiyStatus();
failed = false;
assert.match(element('settings-body').innerHTML, /Loaded in Engines/);
sources = [{ ...sources[0], status: 'stopped', url: null }];
await app.refreshSources();
assert.match(element('readiness').innerHTML, /OAIY Voice is stopped/);
snapshot.lastHealth.components.responder.realtimeError = 'gateway refused';
await app.refreshSnapshot();
assert.match(element('readiness').innerHTML, /last call: gateway refused/);
payloads['settings.get'] = { settings: { ...oaiyBag, realtimeVoiceEndpoint: 'ws://127.0.0.1:17872/api/ai/providers/openai/v1/realtime/stream' } };
delete snapshot.lastHealth.components.responder.realtimeError;
await app.refreshSettings();
assert.match(element('readiness').innerHTML, /Another realtime provider/);
assert.equal(element('settings-title').textContent, 'Calls do not go to OAIY');

// Calls that do not go to OAIY: nobody answers them (Aokie has no speech of
// its own), on the Overview's summary too; the OAIY route reads as before.
payloads['settings.get'] = { settings: { aiReceptionist: true, realtimeVoiceMode: 'legacy' } };
await app.refreshSettings();
assert.match(element('settings-body').innerHTML, /Nobody: calls ring through/);
assert.doesNotMatch(element('settings-body').innerHTML, /own speech/);
payloads['settings.get'] = { settings: oaiyBag };
snapshot = { state: 'running', lastHealth: { status: 'ok', components: { responder: { mode: 'desktop_realtime', ready: true } } } };
await Promise.all([app.refreshSnapshot(), app.refreshSettings()]);
assert.match(element('readiness').innerHTML, /OAIY Front desk/);
assert.doesNotMatch(element('readiness').innerHTML + element('settings-body').innerHTML, /no speech of its own|Nobody/);
assert.equal(element('settings-title').textContent, 'Calls go to OAIY');

const settingsSource = await readFile(new URL('tabs/settings.js', base), 'utf8');
vm.runInContext(settingsSource.replace(/\}\)\(\);\s*$/, `
  window.testSettings = { settingsPatch, withAokieDefaults, personalGreeting, DEFAULT_GREETING, shortAllowedText,
    formFor(bag) { baseline = withAokieDefaults(bag); settings = withAokieDefaults(bag); return formHtml(); },
    noticeAfterBump(changed) { loaded = true; appManaged = false; configVersion = 1; noteConfigVersion(2, false, changed); return appManagedNoticeHtml(); } };
})();`), context);
const settings = window.testSettings;
// Keys the form no longer shows keep their saved values: an edit to the
// greeting patches the greeting and nothing else.
const saved = settings.withAokieDefaults({ ...oaiyBag, aiModel: 'old-model', ttsEngine: 'sherpa', agentHangup: true });
assert.equal(saved.agentHangup, true);
assert.equal(settings.withAokieDefaults({}).agentHangup, false);
assert.equal(JSON.stringify(settings.settingsPatch(saved, { ...saved, greeting: 'Hello' })), JSON.stringify({ greeting: 'Hello' }));
// The greeting box's built-in greeting is the one the plugin says (radio/prompts.rs), and a caller OAIY knows is greeted
// by first name as OAIY Desktop greets them (its voice::callers::personal_greeting tests, the same cases).
const prompts = await readFile(new URL('../crates/aokie-plugin/src/radio/prompts.rs', import.meta.url), 'utf8');
assert.ok(prompts.includes(`pub const DEFAULT_GREETING: &str = "${settings.DEFAULT_GREETING}";`), 'the box shows the plugin\'s own greeting');
for (const [greeting, name, said] of [
  ['Hi! Thanks for calling', 'Liam Smith', 'Hi Liam! Thanks for calling'],
  ['Hello, thanks for calling', 'Liam', 'Hello Liam, thanks for calling'],
  ['hey there, how can I help?', 'Liam', 'hey Liam, how can I help?'],
  ["G'day! OAIY here.", 'Liam', "G'day Liam! OAIY here."],
  ['Thanks for calling OAIY.', 'Liam', 'Hi Liam! Thanks for calling OAIY.'],
  ['Hiya, OAIY here.', 'Liam', 'Hi Liam! Hiya, OAIY here.'],
  ['Hi Liam, thanks for calling', 'Liam Smith', 'Hi Liam, thanks for calling'],
  ['Hi! Thanks for calling', '', 'Hi! Thanks for calling'],
  [settings.DEFAULT_GREETING, 'Zoë', 'Hello Zoë, thanks for calling. How can I help you today?'],
]) assert.equal(settings.personalGreeting(greeting, name), said, greeting);
// The settings form offers no route or setting that needs Aokie's own speech
// (it has none): not Aokie's own receptionist, not its conversation tuning.
// Off OAIY's route it says nobody answers, and offers to send calls to OAIY.
{
  for (const bag of [{ aiReceptionist: true, realtimeVoiceMode: 'legacy' }, { aiReceptionist: false }]) {
    const form = settings.formFor(bag);
    assert.match(form, /no speech of its own/);
    assert.match(form, /they ring through to you/);
    assert.match(form, /data-act="set-route-oaiy"/);
    assert.doesNotMatch(form, /data-bool="aiReceptionist"/);
    assert.doesNotMatch(form, /Conversation tuning|data-num="sttEndpointMs"|data-bool="bargeIn"|data-num="bargeSensitivity"|conversationAcknowledgements/);
  }
  const elsewhere = settings.formFor({ ...oaiyBag, realtimeVoiceEndpoint: 'ws://127.0.0.1:17872/api/ai/providers/openai/v1/realtime/stream' });
  assert.match(elsewhere, /another realtime provider/);
  assert.match(elsewhere, /no voice of its own there/);
  assert.match(elsewhere, /data-act="set-route-oaiy"/);
  const toOaiy = settings.formFor(oaiyBag);
  assert.match(toOaiy, /Calls go to OAIY/);
  assert.doesNotMatch(toOaiy, /no speech of its own|data-act="set-route-oaiy"|data-bool="aiReceptionist"/);
  for (const form of [elsewhere, toOaiy]) {
    assert.doesNotMatch(form, /Conversation tuning|data-num="sttEndpointMs"|data-bool="bargeIn"/);
  }
  // Review 2026-10 wording: the texts switch says what it does (no text at
  // all to another number), the allowed list asks for area codes and names
  // entries too short to match, and with auto-answer off the screen says
  // the refusals do not apply (calls ring through untouched).
  assert.match(toOaiy, /Text only these numbers/);
  assert.doesNotMatch(toOaiy, /Reply to texts only/);
  assert.match(toOaiy, /with its area code/);
  assert.match(toOaiy, /Auto-answer is off/);
  assert.doesNotMatch(settings.formFor({ ...oaiyBag, autoAnswer: true }), /Auto-answer is off/);
  const shortList = settings.formFor({ ...oaiyBag, allowedNumbers: '0400 111 222\n123\n9876 5432' });
  assert.match(shortList, /These match nobody: 123\./);
  assert.match(shortList, /These may miss the caller: 9876 5432\./);
  assert.match(settings.formFor({ ...oaiyBag, allowedNumbers: '0400 111 222' }), /id="set-allowed-short"[^>]* hidden/);
  assert.equal(settings.shortAllowedText('+61 432 602 110, +61 421 285 243'), '');
  // The "changed outside this form" notice: only when the greeting or brief
  // changed under it, never for a bare version bump (any other tab's write).
  assert.equal(settings.noticeAfterBump(false), '');
  assert.match(settings.noticeAfterBump(true), /changed outside this form/);
  assert.doesNotMatch(settings.noticeAfterBump(true), /FormLogic/);
}
// The Consent tab: the plugin's own destinations and verdict decide, and a
// line paused for consent never reads green (review 2026-10).
{
  const consentSource = await readFile(new URL('tabs/consent.js', base), 'utf8');
  const consentHost = { ...host, consent: { get: async () => null, issue: async () => ({}) } };
  const consentWindow = { PluginHost: consentHost, __aokieTabs: window.__aokieTabs, addEventListener() {}, setInterval() {}, clearInterval() {} };
  const consentContext = vm.createContext({ window: consentWindow, document, URL, setTimeout, clearTimeout });
  vm.runInContext(consentSource.replace(/\}\)\(\);\s*$/, `
  window.testConsent = { review, uniqueDestinations, setStatus(s) { status = s; } };
})();`), consentContext);
  const consent = consentWindow.testConsent;
  const grant = { version: 3, scopes: { bluetooth: true, destinations: [] } };
  const good = { grant, requiredVersion: 3, mode: 'enforce', destinations: [], destinationsCovered: true, blocked: null };
  assert.equal(consent.review(good).ok, true);
  assert.equal(consent.review({ ...good, blocked: 'transcription consent denied' }).ok, false);
  assert.equal(consent.review({ ...good, destinationsCovered: false }).ok, false);
  consent.setStatus({ ...good, destinations: ['OpenAI ChatGPT via Codex'] });
  assert.equal(JSON.stringify(consent.uniqueDestinations()), JSON.stringify(['OpenAI ChatGPT via Codex']));
}
const phoneSource = await readFile(new URL('tabs/phone.js', base), 'utf8');
vm.runInContext(phoneSource.replace(/\}\)\(\);\s*$/, `
  window.testPhone = { refreshAll, pairingCardBody, bondedCardBody, confirmedPhoneConnected,
    accept(address) { confirmed = true; confirmedAddress = address; } };
})();`), context);
await window.testPhone.refreshAll();
assert.match(window.testPhone.pairingCardBody(), /Phone connected/);
failed = true;
await window.testPhone.refreshAll();
assert.doesNotMatch(window.testPhone.pairingCardBody(), /Phone connected/);
assert.match(window.testPhone.pairingCardBody(), /could not be verified/);
assert.match(window.testPhone.bondedCardBody(), /Host unavailable/);
failed = false;
await window.testPhone.refreshAll();
assert.match(window.testPhone.pairingCardBody(), /Phone connected/);
// Review 2026-10: an old error the status still carries does not make a
// connected phone read "Pair a phone", on the Phone tab or the Overview.
payloads['phone.status'] = { connected: true, paired: true, error: 'an old link error', device: { name: 'Test phone', address: 'AA:BB' } };
await window.testPhone.refreshAll();
assert.match(window.testPhone.pairingCardBody(), /Phone connected/);
assert.match(window.testPhone.pairingCardBody(), /one phone at a time/);
await app.refreshPhoneStatus();
assert.match(element('readiness').innerHTML, /Bluetooth linked/);
assert.match(element('readiness').innerHTML, /last error: an old link error/);
// A bonded phone that is not connected is paired, offline (not "Not paired").
payloads['phone.status'] = { connected: false, paired: false, device: null };
payloads['phone.listPaired'] = { devices: [{ address: 'AA:BB', connected: false }] };
await Promise.all([app.refreshPhoneStatus(), app.refreshPhones()]);
assert.match(element('readiness').innerHTML, /Paired, offline/);
payloads['phone.listPaired'] = { devices: [] };
await app.refreshPhones();
assert.match(element('readiness').innerHTML, /Not paired/);
// "Phone paired" only for the phone whose code was accepted: another bonded
// phone connecting is not it.
await window.testPhone.refreshAll();
window.testPhone.accept('CC:DD');
payloads['phone.status'] = { connected: true, device: { address: 'AA:BB' } };
await window.testPhone.refreshAll();
assert.equal(window.testPhone.confirmedPhoneConnected(), false);
payloads['phone.status'] = { connected: true, device: { address: 'cc:dd' } };
await window.testPhone.refreshAll();
assert.equal(window.testPhone.confirmedPhoneConnected(), true);
payloads['phone.status'] = { connected: true, paired: true, device: { name: 'Test phone' } };
payloads['phone.listPaired'] = { devices: [{ address: 'test-device', connected: true }] };
// An older OAIY Desktop without navigate/oaiyStatus: links read as text and
// the summary falls back to pointers.
{
  const olderClasses = new Set();
  const olderHost = { ...host, navigate: undefined, oaiyStatus: undefined };
  const olderWindow = { PluginHost: olderHost, addEventListener() {}, setInterval() {}, clearInterval() {} };
  const olderDocument = { ...document, documentElement: { classList: { add: (c) => olderClasses.add(c), remove() {} } } };
  const older = vm.createContext({ window: olderWindow, document: olderDocument, URL, setTimeout, clearTimeout });
  vm.runInContext(appSource.replace(/\}\)\(\);\s*$/, `
  window.testApp = { refreshSettings, refreshOaiyStatus };
})();`), older);
  assert.equal(olderClasses.has('rcp-no-nav'), true);
  payloads['settings.get'] = { settings: oaiyBag };
  await olderWindow.testApp.refreshOaiyStatus();
  await olderWindow.testApp.refreshSettings();
  assert.match(element('settings-body').innerHTML, /tabindex="-1">Chosen in Calendar</);
}
console.log('Receptionist UI checks passed (isolated host; no live commands).');
