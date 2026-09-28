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
};
const window = { PluginHost: host, addEventListener() {}, setInterval() {}, clearInterval() {} };
const document = { hidden: true, getElementById: element, addEventListener() {}, querySelectorAll: () => [] };
const context = vm.createContext({ window, document, URL, setTimeout, clearTimeout });
const appSource = await readFile(new URL('app.js', base), 'utf8');
vm.runInContext(appSource.replace(/\}\)\(\);\s*$/, `
  window.testApp = { state, refreshSnapshot, refreshPhoneStatus, refreshPhones, refreshSettings, refreshSources, refreshSwitchboard, renderLive, replyOwner };
})();`), context);
const app = window.testApp;
app.state.call = { callId: 'fixture', state: 'active' };
await Promise.all([app.refreshSnapshot(), app.refreshPhoneStatus(), app.refreshPhones(), app.refreshSettings()]);
assert.equal(element('hero-headline').textContent, 'Ready for calls');
assert.match(element('readiness').innerHTML, /Bluetooth linked/);
assert.equal(element('speak-send').disabled, false);
snapshot = { state: 'unhealthy', lastHealth: { status: 'degraded', detail: 'Phone is disconnected', components: { responder: { mode: 'agent', ready: true } } }, manifest: { version: '0.2.0' } };
await app.refreshSnapshot();
assert.equal(element('hero-headline').textContent, 'Running — needs attention');
assert.equal(element('hero-sub').textContent, 'Phone is disconnected');
assert.match(element('readiness').innerHTML, /LLM ready/);
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
assert.equal(element('speak-send').disabled, false);
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
assert.match(element('settings-body').innerHTML, /Chosen in OAIY/);
assert.doesNotMatch(element('settings-body').innerHTML, /Pocket|Sherpa|old-model|LLM/);
assert.equal(element('settings-title').textContent, 'Calls go to OAIY');
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

const settingsSource = await readFile(new URL('tabs/settings.js', base), 'utf8');
vm.runInContext(settingsSource.replace(/\}\)\(\);\s*$/, `
  window.testSettings = { settingsPatch, withAokieDefaults };
})();`), context);
const settings = window.testSettings;
// Keys the form no longer shows keep their saved values: an edit to the
// greeting patches the greeting and nothing else.
const saved = settings.withAokieDefaults({ ...oaiyBag, aiModel: 'old-model', ttsEngine: 'sherpa', agentHangup: true });
assert.equal(saved.agentHangup, true);
assert.equal(settings.withAokieDefaults({}).agentHangup, false);
assert.equal(JSON.stringify(settings.settingsPatch(saved, { ...saved, greeting: 'Hello' })), JSON.stringify({ greeting: 'Hello' }));
const phoneSource = await readFile(new URL('tabs/phone.js', base), 'utf8');
vm.runInContext(phoneSource.replace(/\}\)\(\);\s*$/, `
  window.testPhone = { refreshAll, pairingCardBody, bondedCardBody };
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
console.log('Receptionist UI checks passed (isolated host; no live commands).');
