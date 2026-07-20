'use strict';

// Zero-dependency regression harness for the plain-JS receptionist Settings
// tab. Run from the repository root with:
//   node crates/aokie-plugin/tests/settings_ui_harness.cjs

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const settingsScript = fs.readFileSync(
  path.join(__dirname, '..', 'ui', 'receptionist', 'tabs', 'settings.js'),
  'utf8',
);

const CODEX_NONE =
  'http://127.0.0.1:17872/api/ai/providers/openai-codex-agent-none/v1/chat/completions';
const CODEX_LOW =
  'http://127.0.0.1:17872/api/ai/providers/openai-codex-agent-low/v1/chat/completions';
const CODEX_LUNA =
  'http://127.0.0.1:17872/api/ai/providers/openai-codex-agent-luna-low/v1/chat/completions';
const CODEX_LUNA_FAST =
  'http://127.0.0.1:17872/api/ai/providers/openai-codex-agent-luna-low-fast/v1/chat/completions';

let backend = {
  configVersion: 1,
  settings: { aiEndpoint: CODEX_NONE, aiModel: 'gpt-5.5', greeting: 'first' },
};
let sourceListing = [];
let settingsGets = 0;
let sourceGets = 0;
let settingsSets = 0;
let registered;

const listeners = {};
const root = {
  innerHTML: '',
  addEventListener(type, fn) {
    (listeners[type] || (listeners[type] = [])).push(fn);
  },
  querySelector() {
    return null;
  },
  querySelectorAll() {
    return [];
  },
};

function clone(value) {
  return JSON.parse(JSON.stringify(value));
}

const host = {
  command(name, payload) {
    if (name === 'settings.get') {
      settingsGets += 1;
      return Promise.resolve(clone(backend));
    }
    if (name === 'settings.set') {
      settingsSets += 1;
      Object.assign(backend.settings, clone(payload || {}));
      backend.configVersion += 1;
      return Promise.resolve(clone(backend));
    }
    return Promise.reject(new Error('unexpected command: ' + name));
  },
  aiSources() {
    sourceGets += 1;
    return Promise.resolve(clone(sourceListing));
  },
  toast() {},
};

const tabs = {
  util: {
    esc(value) {
      return String(value == null ? '' : value).replace(/[&<>"']/g, (char) => ({
        '&': '&amp;',
        '<': '&lt;',
        '>': '&gt;',
        '"': '&quot;',
        "'": '&#39;',
      })[char]);
    },
    errMsg(error) {
      return error && error.message ? error.message : String(error);
    },
  },
  transport: { update() {} },
  register(id, definition) {
    assert.equal(id, 'settings');
    registered = definition;
  },
};

vm.runInNewContext(settingsScript, {
  URL,
  window: { PluginHost: host, __aokieTabs: tabs },
});

function laneHtml(lane) {
  const match = root.innerHTML.match(new RegExp('<select data-lane="' + lane + '">([\\s\\S]*?)</select>'));
  assert.ok(match, 'rendered ' + lane + ' picker');
  return match[1];
}

function dispatch(type, attributes, value) {
  const target = {
    value,
    getAttribute(name) {
      return Object.prototype.hasOwnProperty.call(attributes, name) ? attributes[name] : null;
    },
  };
  for (const fn of listeners[type] || []) fn({ type, target });
}

function clickAction(action) {
  const button = {
    disabled: false,
    getAttribute(name) {
      return name === 'data-act' ? action : null;
    },
  };
  const target = {
    closest(selector) {
      return selector === '[data-act]' ? button : null;
    },
  };
  for (const fn of listeners.click || []) fn({ target });
}

function settle() {
  return new Promise((resolve) => setImmediate(resolve));
}

(async () => {
  assert.ok(registered, 'Settings tab registered');

  // A persisted Codex URL remains an exact provider selection even when
  // source discovery briefly returns an empty list during Desktop startup.
  registered.mount(root);
  await settle();
  assert.match(
    laneHtml('llm'),
    /value="provider:openai-codex-agent-none" selected/,
    'saved Reasoning off provider is selected',
  );

  // Returning to the tab must fetch both authoritative settings and the live
  // provider list again, rather than repainting the first in-memory snapshot.
  registered.unmount();
  backend = {
    configVersion: 2,
    settings: { aiEndpoint: CODEX_LOW, aiModel: 'gpt-5.5', greeting: 'second' },
  };
  sourceListing = [{
    kind: 'provider',
    id: 'provider:openai-codex-agent-low',
    name: 'ChatGPT via Codex',
    capabilities: ['chat'],
  }];
  registered.mount(root);
  await settle();
  assert.equal(settingsGets, 2, 'settings.get refreshed on tab return');
  assert.equal(sourceGets, 2, 'aiSources refreshed on tab return');
  assert.match(laneHtml('llm'), /value="provider:openai-codex-agent-low" selected/);
  assert.match(root.innerHTML, /data-key="greeting"[^>]*value="second"/);

  // Luna remains exact while provider discovery is unavailable. Its pinned
  // model is truthful, the editable box is hidden, and source precedes model.
  registered.unmount();
  backend = {
    configVersion: 3,
    settings: { aiEndpoint: CODEX_LUNA, aiModel: 'gpt-5.6-luna', greeting: 'luna' },
  };
  sourceListing = [];
  registered.mount(root);
  await settle();
  assert.equal(settingsGets, 3, 'Luna return refreshed settings');
  assert.match(laneHtml('llm'), /value="provider:openai-codex-agent-luna-low" selected/);
  assert.match(root.innerHTML, /id="set-llm-model-zone" hidden/);
  assert.match(root.innerHTML, /id="set-llm-fixed-model"[^>]*>LLM model is fixed to <strong>gpt-5\.6-luna<\/strong>/);
  assert.ok(
    root.innerHTML.indexOf('<select data-lane="llm">') < root.innerHTML.indexOf('id="set-llm-model-zone"'),
    'LLM source renders above its model zone',
  );

  registered.unmount();
  backend = {
    configVersion: 4,
    settings: { aiEndpoint: CODEX_LUNA_FAST, aiModel: 'gpt-5.6-luna', greeting: 'luna fast' },
  };
  registered.mount(root);
  await settle();
  assert.equal(settingsGets, 4, 'Luna Fast return refreshed settings');
  assert.match(laneHtml('llm'), /value="provider:openai-codex-agent-luna-low-fast" selected/);
  assert.match(laneHtml('llm'), /Fast mode/);
  assert.match(root.innerHTML, /id="set-llm-model-zone" hidden/);

  // A provider-id-looking near miss is still a custom endpoint, even while
  // that provider appears in discovery. Saving an unrelated field must not
  // silently turn it into the reserved Codex route.
  registered.unmount();
  const trailingSlashNearMiss = CODEX_NONE + '/';
  backend = {
    configVersion: 5,
    settings: {
      aiEndpoint: trailingSlashNearMiss,
      aiModel: 'custom-model',
      greeting: 'near miss',
    },
  };
  sourceListing = [{
    kind: 'provider',
    id: 'provider:openai-codex-agent-none',
    name: 'ChatGPT via Codex',
    capabilities: ['chat'],
  }];
  registered.mount(root);
  await settle();
  assert.equal(settingsGets, 5, 'near-miss return refreshed settings');
  assert.match(laneHtml('llm'), /value="custom" selected/);
  assert.match(root.innerHTML, /id="set-llm-model-zone">/);
  dispatch('input', { 'data-key': 'greeting' }, 'near miss edited');
  clickAction('set-save');
  await settle();
  assert.equal(settingsSets, 1, 'unrelated near-miss edit saved once');
  assert.equal(
    backend.settings.aiEndpoint,
    trailingSlashNearMiss,
    'unrelated save did not canonicalize a non-reserved route',
  );
  assert.equal(backend.settings.greeting, 'near miss edited');

  // Equivalent spellings all reach the same exact Desktop routes and must
  // hydrate as their provider even when source discovery is unavailable.
  const equivalentRoutes = [
    {
      endpoint: 'http://localhost:17872/api/ai/providers/openai%2Dcodex-agent-none/v1/chat/completions?request=1#ignored',
      source: 'provider:openai-codex-agent-none',
      model: 'gpt-5.5',
    },
    {
      endpoint: 'https://[::1]:17872/api/ai/providers/openai-codex-agent-low/v1/chat/completions',
      source: 'provider:openai-codex-agent-low',
      model: 'gpt-5.5',
    },
    {
      endpoint: 'http://user:pass@127.0.0.1:17872/api/ai/providers/openai-codex-agent-luna-low-fast/v1/chat/completions',
      source: 'provider:openai-codex-agent-luna-low-fast',
      model: 'gpt-5.6-luna',
    },
  ];
  for (let i = 0; i < equivalentRoutes.length; i += 1) {
    const route = equivalentRoutes[i];
    registered.unmount();
    backend = {
      configVersion: 6 + i,
      settings: { aiEndpoint: route.endpoint, aiModel: route.model, greeting: 'equivalent ' + i },
    };
    sourceListing = [];
    registered.mount(root);
    await settle();
    assert.match(
      laneHtml('llm'),
      new RegExp('value="' + route.source + '" selected'),
      'equivalent route hydrated: ' + route.endpoint,
    );
    assert.match(root.innerHTML, /id="set-llm-model-zone" hidden/);
  }

  // An automatic refresh may finish after the operator made local edits. It
  // must keep both ordinary fields and lane picks, and must never save them.
  dispatch('input', { 'data-key': 'greeting' }, 'local & unsaved');
  dispatch('change', { 'data-lane': 'llm' }, 'provider:openai-codex-agent-none');
  registered.unmount();
  backend = {
    configVersion: 9,
    settings: { aiEndpoint: '', aiModel: '', greeting: 'third from backend' },
  };
  sourceListing = [];
  const getsBeforeDirtyReturn = settingsGets;
  const sourceGetsBeforeDirtyReturn = sourceGets;
  const setsBeforeDirtyReturn = settingsSets;
  registered.mount(root);
  await settle();
  assert.equal(settingsGets, getsBeforeDirtyReturn + 1, 'dirty return still performs the backend read');
  assert.equal(sourceGets, sourceGetsBeforeDirtyReturn + 1, 'dirty return still refreshes sources');
  assert.match(root.innerHTML, /data-key="greeting"[^>]*value="local &amp; unsaved"/);
  assert.match(laneHtml('llm'), /value="provider:openai-codex-agent-none" selected/);
  assert.equal(settingsSets, setsBeforeDirtyReturn, 'mount/refresh never triggers settings.set');

  console.log('settings UI harness: ok');
})().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
