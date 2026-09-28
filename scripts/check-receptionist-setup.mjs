import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

// Setup mode of the receptionist screen: the screen as a step of OAIY's setup
// wizard (PluginHost.setup), next to the normal screen (no PluginHost.setup).
// Runs index.html's elements and every screen script, in manifest order, with
// an isolated host. No device, network or plugin is touched.
const crate = new URL('../crates/aokie-plugin/', import.meta.url);
const manifest = JSON.parse(await readFile(new URL('manifest.json', crate), 'utf8'));
const screenFiles = manifest.ui.screens[0].files;
const scripts = await Promise.all(
  screenFiles.filter((f) => f.endsWith('.js')).map(async (f) => ({ file: f, source: await readFile(new URL(f, crate), 'utf8') })),
);
const indexHtml = await readFile(new URL('ui/receptionist/index.html', crate), 'utf8');

const clone = (v) => (v === undefined ? v : JSON.parse(JSON.stringify(v)));
const settle = async (turns = 8) => {
  for (let i = 0; i < turns; i++) await new Promise((resolve) => setImmediate(resolve));
};

// ---- a small DOM: the elements index.html gives an id --------------------

function classList(initial) {
  const set = new Set(initial);
  return {
    add: (...names) => names.forEach((n) => set.add(n)),
    remove: (...names) => names.forEach((n) => set.delete(n)),
    toggle(name, on) {
      const next = on === undefined ? !set.has(name) : !!on;
      if (next) set.add(name);
      else set.delete(name);
      return next;
    },
    contains: (name) => set.has(name),
  };
}

function element(id, attrs = {}) {
  const listeners = {};
  return {
    id,
    attrs,
    hidden: Object.hasOwn(attrs, 'hidden'),
    innerHTML: '',
    textContent: '',
    value: '',
    disabled: false,
    tabIndex: 0,
    listeners,
    classList: classList(String(attrs.class || '').split(/\s+/).filter(Boolean)),
    addEventListener(type, fn) {
      (listeners[type] || (listeners[type] = [])).push(fn);
    },
    setAttribute(name, value) {
      attrs[name] = String(value);
    },
    getAttribute(name) {
      return Object.hasOwn(attrs, name) ? attrs[name] : null;
    },
    insertAdjacentHTML(_where, html) {
      this.innerHTML += html;
    },
    focus() {},
    scrollIntoView() {},
  };
}

function parseElements() {
  const out = new Map();
  for (const tag of indexHtml.matchAll(/<[a-z][a-z0-9]*\b([^>]*)>/gi)) {
    const attrs = {};
    for (const a of tag[1].matchAll(/([\w-]+)(?:="([^"]*)")?/g)) attrs[a[1]] = a[2] === undefined ? '' : a[2];
    if (attrs.id) out.set(attrs.id, element(attrs.id, attrs));
  }
  return out;
}

/** A target for a delegated click: `closest(selector)` finds `hit` when
 *  it matches the selector's attribute. */
function clickTarget(attrs) {
  const hit = { disabled: false, getAttribute: (n) => (Object.hasOwn(attrs, n) ? attrs[n] : null) };
  return {
    closest(selector) {
      const name = selector.replace(/^\[|\]$/g, '');
      return Object.hasOwn(attrs, name) ? hit : null;
    },
  };
}

// ---- the page --------------------------------------------------------------

/** Load the screen. `setup` (optional) becomes PluginHost.setup; `payloads`
 *  answer PluginHost.command (a function is called with the payload; an
 *  Error rejects). Every host call is logged in `log`. */
async function openScreen({ setup, payloads = {}, restartPlugin, hidden = false } = {}) {
  const log = [];
  const intervals = [];
  const timeouts = [];
  let eventCallback = null;
  const host = {
    command: async (name, payload) => {
      log.push(['command', name, clone(payload)]);
      if (!Object.hasOwn(payloads, name)) throw new Error('unexpected command ' + name);
      const answer = typeof payloads[name] === 'function' ? payloads[name](payload) : payloads[name];
      if (answer instanceof Error) throw answer;
      return clone(answer);
    },
    snapshot: async () => {
      log.push(['snapshot']);
      return { state: 'running', lastHealth: { status: 'ok' } };
    },
    aiSources: async () => {
      log.push(['aiSources']);
      return [];
    },
    oaiyStatus: async () => null,
    navigate: async () => true,
    toast: (kind, message) => {
      log.push(['toast', kind, message]);
    },
    events: {
      subscribe: async (_names, cb) => {
        eventCallback = cb;
        return { unsubscribe() {} };
      },
    },
    consent: {
      get: () => host.command('consent.get'),
      issue: (grant) => host.command('consent.set', grant),
      revoke: () => host.command('consent.revoke'),
    },
  };
  if (restartPlugin) host.restartPlugin = restartPlugin;
  if (setup) host.setup = setup;

  const elements = parseElements();
  const docListeners = {};
  const document = {
    readyState: 'loading',
    hidden,
    activeElement: null,
    documentElement: element('html'),
    body: element('body'),
    addEventListener(type, fn) {
      (docListeners[type] || (docListeners[type] = [])).push(fn);
    },
    getElementById: (id) => elements.get(id) || null,
    querySelectorAll: (selector) =>
      selector === '.rcp-tab-btn' ? [...elements.values()].filter((e) => e.classList.contains('rcp-tab-btn')) : [],
  };
  const window = {
    PluginHost: host,
    addEventListener() {},
    setInterval: (fn, ms) => {
      intervals.push(ms);
      return intervals.length;
    },
    clearInterval() {},
    setTimeout: (fn, ms) => {
      timeouts.push({ fn, ms });
      return timeouts.length;
    },
    clearTimeout() {},
  };
  const context = vm.createContext({ window, document, URL, setTimeout, clearTimeout });

  // As in the host's iframe: one script per file, microtasks between them,
  // then DOMContentLoaded once every script has run.
  for (const { file, source } of scripts) {
    vm.runInContext(source, context, { filename: file });
    await Promise.resolve();
    await Promise.resolve();
  }
  const beforeReady = {
    tabsVisible: ['consent', 'dongle', 'phone', 'settings', 'companion'].filter((t) => !elements.get('tab-' + t).hidden),
    rootSetup: elements.get('rcp-root').classList.contains('is-setup'),
    intervals: intervals.slice(),
  };
  document.readyState = 'interactive';
  for (const fn of docListeners.DOMContentLoaded || []) fn();
  await settle();

  const el = (id) => elements.get(id);
  return {
    log,
    intervals,
    timeouts,
    beforeReady,
    el,
    tabs: window.__aokieTabs,
    commands: () => log.filter((l) => l[0] === 'command').map((l) => l[1]),
    visibleTabs: () => ['overview', 'consent', 'dongle', 'phone', 'settings', 'companion'].filter((t) => !el('tab-' + t).hidden),
    /** Click a [data-act] button inside a tab panel. */
    async click(panel, act, extra = {}) {
      for (const fn of el('tab-' + panel).listeners.click || []) fn({ target: clickTarget({ 'data-act': act, ...extra }) });
      await settle();
    },
    /** Click an element carrying `attrs` anywhere (the document's delegation). */
    async clickDocument(attrs) {
      for (const fn of docListeners.click || []) fn({ target: clickTarget(attrs) });
      await settle();
    },
    async event(name) {
      eventCallback({ name });
      await settle();
    },
  };
}

/** A PluginHost.setup that logs every call. */
function setupStub(ctx, overrides = {}) {
  const calls = [];
  const stub = {
    calls,
    context: () => {
      calls.push(['context']);
      return Promise.resolve(ctx);
    },
    progress: (fraction, text) => {
      calls.push(['progress', fraction, text]);
      return Promise.resolve();
    },
    done: (detail) => {
      calls.push(['done', detail]);
      return Promise.resolve();
    },
    fail: (message) => {
      calls.push(['fail', message]);
      return Promise.resolve();
    },
    finish: () => {
      calls.push(['finish']);
      return Promise.resolve();
    },
    ...overrides,
  };
  return stub;
}

const doneCalls = (stub) => stub.calls.filter((c) => c[0] === 'done');
const failCalls = (stub) => stub.calls.filter((c) => c[0] === 'fail');
const OVERVIEW_ONLY = ['call.current', 'call.switchboard', 'phone.listPaired', 'dongle.diagnostics', 'phone.status'];

const grant = {
  version: 1,
  scopes: { bluetooth: true, transcription: true, sms: true, contacts: true, recording: false, retentionDays: 90, destinations: [] },
  acceptedAt: '2026-09-29T00:00:00Z',
  acceptedBy: 'operator',
  expiresAt: '2027-09-29T00:00:00Z',
};
const consentOk = { grant, signed: true, note: null, requiredVersion: 1, mode: 'enforce', blocked: null };
const consentNone = { grant: null, signed: false, note: null, requiredVersion: 1, mode: 'enforce', blocked: 'no consent has been recorded for this device' };
const boundDongle = { vid: 2652, pid: 8684, vidHex: '0a5c', pidHex: '21ec', description: 'Broadcom BCM20702', matchesCatalog: true, driverBound: true };
const radioUp = { radio: { initialized: true, connected: false, localAddress: '00:1A:7D:DA:71:13' }, outbox: { pending: 0, failed: 0, dead: 0 } };
const radioDown = { radio: { initialized: false, connected: false, localAddress: null, error: 'radio failed to start' }, outbox: { pending: 0, failed: 0, dead: 0 } };

// ---- normal mode: exactly as before ------------------------------------------

{
  const page = await openScreen({
    payloads: {
      'phone.status': { connected: true, paired: true },
      'phone.listPaired': { devices: [] },
      'dongle.diagnostics': radioUp,
      'call.current': { call: null },
      'call.switchboard': { waiting: null, parked: null },
      'settings.get': { settings: {} },
    },
  });
  // The Overview's timers start at once, as they always have, and the
  // screen never waits for DOMContentLoaded.
  assert.deepEqual(page.beforeReady.intervals, [5000, 2000]);
  assert.equal(page.beforeReady.rootSetup, false);
  assert.equal(page.el('rcp-root').classList.contains('is-setup'), false);
  assert.equal(page.el('rcp-tabs').hidden, false);
  assert.deepEqual(page.visibleTabs(), ['overview']);
  assert.ok(page.log.some((l) => l[0] === 'snapshot'), 'the Overview reads the snapshot');
  assert.equal(page.tabs.setup.active(), false);
  assert.equal(await page.tabs.setup.done('x'), false, 'done() is a no-op outside setup mode');
  assert.equal(await page.tabs.setup.fail('x'), false);
  // The tab bar still switches tabs.
  await page.clickDocument({ 'data-tab': 'phone' });
  assert.deepEqual(page.visibleTabs(), ['phone']);
}

// ---- setup mode: consent already recorded --------------------------------------

{
  const setup = setupStub({ mode: 'setup', step: 'consent', view: 'consent' });
  const page = await openScreen({
    setup,
    payloads: { 'consent.get': consentOk, 'settings.get': { settings: {} } },
  });
  // The chrome is hidden before the tabs load; the tab mounts only once
  // every tab script has registered.
  assert.equal(page.beforeReady.rootSetup, true);
  assert.deepEqual(page.beforeReady.tabsVisible, []);
  assert.deepEqual(page.beforeReady.intervals, []);
  assert.equal(page.el('rcp-root').classList.contains('is-setup'), true);
  assert.equal(page.el('rcp-tabs').hidden, true);
  assert.deepEqual(page.visibleTabs(), ['consent'], 'only the named tab renders');
  assert.deepEqual(page.intervals, [], 'no Overview timers in setup mode');
  assert.ok(!page.log.some((l) => l[0] === 'snapshot'), 'no Overview reads in setup mode');
  assert.deepEqual(page.commands().filter((c) => OVERVIEW_ONLY.includes(c)), []);
  assert.match(page.el('tab-consent').innerHTML, /Consent v1 · enforced/);
  assert.deepEqual(doneCalls(setup), [['done', 'Consent v1 recorded and enforced']]);
  assert.equal(page.tabs.setup.step(), 'consent');
  // The tab bar and data-tabgo links do nothing in the wizard.
  await page.clickDocument({ 'data-tabgo': 'phone' });
  await page.clickDocument({ 'data-tab': 'overview' });
  assert.deepEqual(page.visibleTabs(), ['consent']);
}

// ---- setup mode: consent to give, through the tab's own form ------------------

{
  const setup = setupStub({ mode: 'setup', step: 'consent', view: 'consent' });
  let consent = consentNone;
  const page = await openScreen({
    setup,
    payloads: {
      'consent.get': () => consent,
      'consent.set': (g) => {
        consent = { ...consentOk, grant: { ...grant, scopes: g.scopes } };
        return { recorded: true };
      },
      'settings.get': { settings: {} },
      'settings.set': { configVersion: 2, settings: { consentMode: 'enforce' } },
    },
  });
  assert.match(page.el('tab-consent').innerHTML, /Accept &amp; enforce|Accept & enforce/, 'the consent form opens at once');
  assert.deepEqual(doneCalls(setup), []);
  await page.click('consent', 'cns-accept');
  assert.ok(page.commands().includes('consent.set'));
  assert.ok(page.commands().includes('settings.set'));
  assert.deepEqual(doneCalls(setup), [['done', 'Consent v1 recorded and enforced']], 'done() once the grant is saved');
  assert.deepEqual(failCalls(setup), []);
}

// ---- a host whose setup.done throws, and one without progress/fail -------------

{
  const setup = setupStub(
    { mode: 'setup', step: 'consent', view: 'consent' },
    {
      done: () => {
        throw new Error('boom');
      },
      progress: undefined,
      fail: undefined,
    },
  );
  let consent = consentNone;
  const page = await openScreen({
    setup,
    payloads: {
      'consent.get': () => consent,
      'consent.set': () => {
        consent = consentOk;
        return {};
      },
      'settings.get': { settings: {} },
      'settings.set': { settings: {} },
    },
  });
  await page.click('consent', 'cns-accept');
  // The accept flow finished (the form closed, nothing stuck on "Recording…").
  assert.doesNotMatch(page.el('tab-consent').innerHTML, /Recording…/);
  assert.match(page.el('tab-consent').innerHTML, /Consent v1 · enforced/);
}

// ---- setup mode: phone pairing --------------------------------------------------

{
  const setup = setupStub({ mode: 'setup', step: 'pair', view: 'phone' });
  // The ACL is up a beat before the numeric comparison: not paired yet.
  let status = { connected: true, paired: true, device: { address: 'AA:BB', name: 'Pixel' }, pairingOpen: true, pairingSecondsRemaining: 120, pairingConfirm: null };
  let bonds = { devices: [] };
  const page = await openScreen({
    setup,
    payloads: {
      'phone.status': () => status,
      'phone.listPaired': () => bonds,
      'settings.get': { settings: {} },
      'phone.stopPairing': {},
    },
  });
  assert.deepEqual(page.visibleTabs(), ['phone']);
  assert.match(page.el('tab-phone').innerHTML, /Pairing window open/);
  assert.deepEqual(doneCalls(setup), [], 'a connection mid-pairing is not done');
  // The phone bonds; the window closes.
  status = { ...status, pairingOpen: false, pairingSecondsRemaining: 0 };
  bonds = { devices: [{ address: 'AA:BB', name: 'Pixel', connected: true }] };
  await page.event('aokie.phone.paired');
  assert.deepEqual(doneCalls(setup), [['done', 'Paired with Pixel']]);
  await page.event('aokie.phone.connected');
  assert.equal(doneCalls(setup).length, 1, 'done() is told once');
  // Lost and back: told again.
  status = { ...status, connected: false, device: null };
  bonds = { devices: [{ address: 'AA:BB', name: 'Pixel', connected: false }] };
  await page.event('aokie.phone.disconnected');
  status = { ...status, connected: true, device: { address: 'AA:BB', name: 'Pixel' } };
  bonds = { devices: [{ address: 'AA:BB', name: 'Pixel', connected: true }] };
  await page.event('aokie.phone.connected');
  assert.equal(doneCalls(setup).length, 2);
}

{
  // Re-pairing a phone that is already bonded: its link is up and it is in
  // the bond list, but the window is open, so the pairing is not finished.
  const setup = setupStub({ mode: 'setup', step: 'pair', view: 'phone' });
  let status = { connected: true, paired: true, device: { address: 'AA:BB', name: 'Pixel' }, pairingOpen: true, pairingSecondsRemaining: 90, pairingConfirm: null };
  const page = await openScreen({
    setup,
    payloads: {
      'phone.status': () => status,
      'phone.listPaired': { devices: [{ address: 'AA:BB', name: 'Pixel', connected: true }] },
      'settings.get': { settings: {} },
    },
  });
  assert.deepEqual(doneCalls(setup), []);
  status = { ...status, pairingConfirm: { address: 'AA:BB', numericValue: 123456 } };
  await page.event('aokie.phone.pairing_confirm_required');
  assert.match(page.el('tab-phone').innerHTML, /123456/);
  assert.deepEqual(doneCalls(setup), [], 'not done while the numeric comparison is open');
}

{
  // Already paired when the step opens.
  const setup = setupStub({ mode: 'setup', step: 'pair', view: 'phone' });
  const page = await openScreen({
    setup,
    payloads: {
      'phone.status': { connected: true, paired: true, device: { address: 'AA:BB', name: 'Pixel' }, pairingOpen: false, pairingSecondsRemaining: 0 },
      'phone.listPaired': { devices: [{ address: 'AA:BB', name: 'Pixel', connected: true }] },
      'settings.get': { settings: {} },
    },
  });
  assert.deepEqual(doneCalls(setup), [['done', 'Paired with Pixel']]);
  assert.deepEqual(page.intervals, [12000], 'only the tab’s own idle poll');
}

// ---- setup mode: the dongle ------------------------------------------------------

{
  // The radio is up on the dongle already.
  const setup = setupStub({ mode: 'setup', step: 'dongle', view: 'dongle' });
  const page = await openScreen({
    setup,
    payloads: { 'settings.get': { settings: {} }, 'dongle.diagnostics': radioUp, 'dongle.list': { connected: [boundDongle] } },
  });
  assert.deepEqual(page.visibleTabs(), ['dongle']);
  assert.deepEqual(doneCalls(setup), [['done', 'Dongle ready (00:1A:7D:DA:71:13)']]);
  assert.match(page.el('tab-dongle').innerHTML, /radio is up on the dongle/);
  assert.doesNotMatch(page.el('tab-dongle').innerHTML, /Reset the dongle/, 'no recovery card in the wizard');
  assert.ok(setup.calls.some((c) => c[0] === 'progress' && c[1] === 1));
  // Every done check the step reads is sent with no payload (diagnostics'
  // `simulate` must never be set).
  for (const l of page.log) if (l[1] === 'dongle.diagnostics') assert.equal(l[2], undefined);
}

{
  // The driver is on, but the radio started before it: start Aokie again.
  const setup = setupStub({ mode: 'setup', step: 'dongle', view: 'dongle' });
  let diagnostics = radioDown;
  let restarts = 0;
  const page = await openScreen({
    setup,
    payloads: { 'settings.get': { settings: {} }, 'dongle.diagnostics': () => diagnostics, 'dongle.list': { connected: [boundDongle] } },
    restartPlugin: async () => {
      restarts += 1;
      diagnostics = radioUp;
      return true;
    },
  });
  assert.deepEqual(doneCalls(setup), []);
  assert.match(page.el('tab-dongle').innerHTML, /data-act="dg-setup-start"/);
  await page.click('dongle', 'dg-setup-start');
  assert.equal(restarts, 1);
  assert.deepEqual(doneCalls(setup), [['done', 'Dongle ready (00:1A:7D:DA:71:13)']]);
  assert.doesNotMatch(page.el('tab-dongle').innerHTML, /Starting Aokie/);
}

{
  // An older host without restartPlugin: an honest pointer, nothing stuck.
  const setup = setupStub({ mode: 'setup', step: 'dongle', view: 'dongle' });
  const page = await openScreen({
    setup,
    payloads: { 'settings.get': { settings: {} }, 'dongle.diagnostics': new Error('the radio is not running'), 'dongle.list': { connected: [boundDongle] } },
  });
  await page.click('dongle', 'dg-setup-start');
  assert.match(page.el('tab-dongle').innerHTML, /cannot restart Aokie from here/);
  assert.doesNotMatch(page.el('tab-dongle').innerHTML, /disabled>Starting Aokie/);
  assert.deepEqual(doneCalls(setup), []);
  assert.deepEqual(failCalls(setup), [], 'a retryable error stays in the tab');
}

{
  // Consent keeps the radio off: no restart offer, the reason instead.
  const setup = setupStub({ mode: 'setup', step: 'dongle', view: 'dongle' });
  const paused = {
    radio: { running: false, status: 'paused', paused: true, blockedBy: 'consent', reason: 'no consent has been recorded for this device', initialized: false, connected: false },
    outbox: { pending: 0, failed: 0, dead: 0 },
  };
  const page = await openScreen({
    setup,
    payloads: { 'settings.get': { settings: {} }, 'dongle.diagnostics': paused, 'dongle.list': { connected: [boundDongle] } },
    restartPlugin: async () => {
      throw new Error('must not restart');
    },
  });
  assert.doesNotMatch(page.el('tab-dongle').innerHTML, /data-act="dg-setup-start"/);
  assert.match(page.el('tab-dongle').innerHTML, /off until consent is given: no consent has been recorded/);
  assert.deepEqual(doneCalls(setup), []);
}

{
  // Built-in Windows Bluetooth: no dongle to set up, and the view stays.
  const setup = setupStub({ mode: 'setup', step: 'dongle', view: 'dongle' });
  const page = await openScreen({
    setup,
    payloads: { 'settings.get': { settings: { transportMode: 'native' } }, 'dongle.diagnostics': radioDown, 'dongle.list': { connected: [] } },
  });
  assert.deepEqual(page.visibleTabs(), ['dongle']);
  assert.match(page.el('tab-dongle').innerHTML, /No dongle needed/);
  assert.deepEqual(doneCalls(setup), [['done', 'Built-in Bluetooth: no dongle needed']]);
}

// ---- setup mode: a view this screen does not have -------------------------------

{
  const setup = setupStub({ mode: 'setup', step: 'voice', view: 'voice' });
  const page = await openScreen({ setup });
  assert.deepEqual(page.visibleTabs(), []);
  assert.equal(failCalls(setup).length, 1);
  assert.match(page.el('rcp-root').innerHTML, /no setup view named/);
  // The hidden Overview does not wake up on live events.
  await page.event('aokie.phone.connected');
  assert.deepEqual(page.commands(), []);
}

// ---- PluginHost.setup present, but not a setup context: the normal screen ------

{
  const setup = setupStub({ mode: 'normal' });
  const page = await openScreen({
    setup,
    payloads: {
      'phone.status': { connected: true },
      'phone.listPaired': { devices: [] },
      'dongle.diagnostics': radioUp,
      'call.current': { call: null },
      'call.switchboard': { waiting: null, parked: null },
      'settings.get': { settings: {} },
    },
  });
  assert.equal(page.el('rcp-root').classList.contains('is-setup'), false);
  assert.deepEqual(page.visibleTabs(), ['overview']);
  assert.deepEqual(page.intervals, [5000, 2000]);
  assert.equal(page.tabs.setup.active(), false);
}

console.log('Receptionist setup-mode checks passed (isolated host; no live commands).');
