/*
 * Settings tab — the editable form over Aokie's live connector settings.
 *
 * Faithful port of FormLogic Desktop's compiled AokieSettingsForm
 * (desktop/src/aokie/AokieCard.tsx) + the pure helpers from
 * desktop/src/aokie/aokieSettings.ts (names/semantics preserved — a parity
 * suite may retarget the cross-repo settings tests at these functions).
 *
 * Reads via `settings.get`, writes ONLY the fields the operator changed via
 * `settings.set` (the plugin merges per key, so untouched safety keys like
 * autoAnswer and unknown/newer plugin keys are never rewritten by an
 * unrelated edit — audit AOK-SAFE-001). No plugin restart required.
 */
(function () {
  'use strict';

  var HOST = window.PluginHost;
  var TABS = window.__aokieTabs;
  if (!HOST || !TABS) return;
  var U = TABS.util;
  var esc = U.esc;
  var errMsg = U.errMsg;

  // ======================================================================
  // Ported pure helpers — desktop/src/aokie/aokieSettings.ts (TS → JS).
  // Keep function names and semantics in lock-step with that module.
  // ======================================================================

  var AOKIE_SETTINGS_DEFAULTS = {
    aiReceptionist: false,
    autoAnswer: false,
    answerTone: false,
    greeting: '',
    persona: '',
    ttsVoice: '',
    ttsEngine: '',
    ttsModelDir: '',
    aiModel: '',
    aiEndpoint: '',
    sttEndpoint: '',
    ttsEndpoint: '',
    sttEndpointMs: 450,
    bargeIn: false,
    sendAudio: false,
    bargeSensitivity: 650,
    hfpCodec: 'auto',
    transportMode: 'dongle',
    reenumerateHwid: '',
    legacyPairingPin: false,
  };

  /** Parse a boolean setting the way the plugin does (legacy "true"/"false"
   *  string forms coerce; anything else falls back). */
  function boolSetting(value, fallback) {
    if (typeof value === 'boolean') return value;
    if (value === 'true') return true;
    if (value === 'false') return false;
    return fallback;
  }

  /** Merge a raw settings bag over the defaults so every field is defined,
   *  tolerating missing/mistyped keys from an older plugin build. */
  function withAokieDefaults(raw) {
    var src = raw && typeof raw === 'object' ? raw : {};
    var d = AOKIE_SETTINGS_DEFAULTS;
    var codec = src.hfpCodec;
    return {
      aiReceptionist: boolSetting(src.aiReceptionist, d.aiReceptionist),
      autoAnswer: boolSetting(src.autoAnswer, d.autoAnswer),
      answerTone: boolSetting(src.answerTone, d.answerTone),
      greeting: typeof src.greeting === 'string' ? src.greeting : d.greeting,
      persona: typeof src.persona === 'string' ? src.persona : d.persona,
      ttsVoice: typeof src.ttsVoice === 'string' ? src.ttsVoice : d.ttsVoice,
      ttsEngine: typeof src.ttsEngine === 'string' ? src.ttsEngine : d.ttsEngine,
      ttsModelDir: typeof src.ttsModelDir === 'string' ? src.ttsModelDir : d.ttsModelDir,
      aiModel: typeof src.aiModel === 'string' ? src.aiModel : d.aiModel,
      aiEndpoint: typeof src.aiEndpoint === 'string' ? src.aiEndpoint : d.aiEndpoint,
      sttEndpoint: typeof src.sttEndpoint === 'string' ? src.sttEndpoint : d.sttEndpoint,
      ttsEndpoint: typeof src.ttsEndpoint === 'string' ? src.ttsEndpoint : d.ttsEndpoint,
      sttEndpointMs:
        typeof src.sttEndpointMs === 'number' && isFinite(src.sttEndpointMs)
          ? src.sttEndpointMs
          : d.sttEndpointMs,
      bargeIn: boolSetting(src.bargeIn, d.bargeIn),
      sendAudio: boolSetting(src.sendAudio, d.sendAudio),
      bargeSensitivity:
        typeof src.bargeSensitivity === 'number' && isFinite(src.bargeSensitivity)
          ? src.bargeSensitivity
          : d.bargeSensitivity,
      hfpCodec: codec === 'cvsd' || codec === 'wbs' || codec === 'auto' ? codec : d.hfpCodec,
      transportMode:
        src.transportMode === 'native' || src.transportMode === 'auto' || src.transportMode === 'dongle'
          ? src.transportMode
          : d.transportMode,
      reenumerateHwid:
        typeof src.reenumerateHwid === 'string' ? src.reenumerateHwid : d.reenumerateHwid,
      legacyPairingPin: boolSetting(src.legacyPairingPin, d.legacyPairingPin),
    };
  }

  /** The dirty-field patch for a save: only keys whose value differs from
   *  the baseline. Empty object = nothing changed (skip the write). */
  function settingsPatch(baseline, current) {
    var patch = {};
    var keys = Object.keys(current);
    for (var i = 0; i < keys.length; i++) {
      var key = keys[i];
      if (current[key] !== baseline[key]) patch[key] = current[key];
    }
    return patch;
  }

  /** Conventional OpenAI-compatible path per lane. */
  var LANE_PATHS = {
    llm: '/v1/chat/completions',
    stt: '/v1/audio/transcriptions',
    tts: '/v1/audio/speech',
  };

  /** The desktop AI gateway's FIXED loopback base (lock-step with the
   *  console's receptionistPayload.ts AI_GATEWAY_BASE). */
  var AI_GATEWAY_BASE = 'http://127.0.0.1:17872/api/ai/providers/';
  var CODEX_PROVIDER_LUNA_LOW = 'openai-codex-agent-luna-low';
  var CODEX_PROVIDER_LUNA_LOW_FAST = 'openai-codex-agent-luna-low-fast';
  var CODEX_PROVIDER_NONE = 'openai-codex-agent-none';
  var CODEX_PROVIDER_LOW = 'openai-codex-agent-low';
  var CODEX_MODEL = 'gpt-5.5';
  var CODEX_LUNA_MODEL = 'gpt-5.6-luna';

  /** Reserved live-call variants. Only their exact provider paths receive
   *  this policy; ordinary providers behind the same gateway stay ordinary. */
  function codexVariant(providerId) {
    if (providerId === CODEX_PROVIDER_LUNA_LOW) return 'GPT-5.6 Luna · low reasoning (fastest Luna)';
    if (providerId === CODEX_PROVIDER_LUNA_LOW_FAST) {
      return 'GPT-5.6 Luna · low reasoning · Fast mode';
    }
    if (providerId === CODEX_PROVIDER_NONE) return 'GPT-5.5 · reasoning off';
    if (providerId === CODEX_PROVIDER_LOW) return 'GPT-5.5 · low reasoning';
    return null;
  }

  function codexModel(providerId) {
    if (providerId === CODEX_PROVIDER_LUNA_LOW || providerId === CODEX_PROVIDER_LUNA_LOW_FAST) {
      return CODEX_LUNA_MODEL;
    }
    if (providerId === CODEX_PROVIDER_NONE || providerId === CODEX_PROVIDER_LOW) return CODEX_MODEL;
    return null;
  }

  function codexVariantForSource(source) {
    var src = String(source || '');
    return src.indexOf('provider:') === 0 ? codexVariant(src.slice(9)) : null;
  }

  function codexModelForSource(source) {
    var src = String(source || '');
    return src.indexOf('provider:') === 0 ? codexModel(src.slice(9)) : null;
  }

  function codexProviderForEndpoint(url) {
    try {
      var parsed = new URL(String(url || '').trim());
      var host = parsed.hostname.toLowerCase();
      var ipHost = host[0] === '[' && host[host.length - 1] === ']' ? host.slice(1, -1) : host;
      var segments = parsed.pathname.split('/');
      if (
        segments.length !== 8 ||
        segments[0] !== '' ||
        segments[1] !== 'api' ||
        segments[2] !== 'ai' ||
        segments[3] !== 'providers' ||
        segments[5] !== 'v1' ||
        segments[6] !== 'chat' ||
        segments[7] !== 'completions' ||
        /%(?:2f|5c|00)/i.test(segments[4])
      ) {
        return null;
      }
      var providerId = decodeURIComponent(segments[4]);
      var loopback =
        host === 'localhost' ||
        ipHost === '::1' ||
        ipHost === '::' ||
        /^::ffff:7f[0-9a-f]{2}:[0-9a-f]{1,4}$/.test(ipHost) ||
        ipHost === '::ffff:0:0' ||
        /^127(?:\.[0-9]{1,3}){3}$/.test(host) ||
        host === '0.0.0.0';
      return (
        loopback &&
        (parsed.protocol === 'http:' || parsed.protocol === 'https:') &&
        parsed.port === '17872' &&
        codexModel(providerId) &&
        providerId
      ) || null;
    } catch (e) {
      return null;
    }
  }

  function codexModelForEndpoint(url) {
    var providerId = codexProviderForEndpoint(url);
    return providerId ? codexModel(providerId) : null;
  }

  /** Compose one lane's saved endpoint URL from a source pick (same rule as
   *  the console's laneUrl; providerOk = the LLM lane only). */
  function composeLaneUrl(source, custom, lane, sources) {
    var src = String(source || '').trim();
    var url = String(custom || '').trim();
    if (!src || src === 'custom') return url;
    if (src.indexOf('service:') === 0) {
      var svc = null;
      for (var i = 0; i < sources.length; i++) {
        if (sources[i].kind === 'service' && sources[i].id === src) {
          svc = sources[i];
          break;
        }
      }
      if (!svc || svc.status !== 'running' || !svc.url) return '';
      return svc.url + LANE_PATHS[lane];
    }
    if (src.indexOf('provider:') === 0) {
      if (lane !== 'llm') return '';
      return AI_GATEWAY_BASE + encodeURIComponent(src.slice(9)) + LANE_PATHS[lane];
    }
    return url;
  }

  /** Reverse of composeLaneUrl for seeding the select from a saved URL. */
  function inferLaneSource(savedUrl, lane, sources) {
    var url = String(savedUrl || '').trim();
    if (!url) return '';
    // The four Desktop-owned Codex adapters accept equivalent loopback URL
    // spellings. Hydrate them through the exact route parser before the
    // canonical-prefix fallback below, so localhost/IPv6/userinfo/encoded-id
    // forms remain the same provider selection.
    if (lane === 'llm') {
      var codexProviderId = codexProviderForEndpoint(url);
      if (codexProviderId) return 'provider:' + codexProviderId;
    }
    var path = LANE_PATHS[lane];
    for (var i = 0; i < sources.length; i++) {
      var x = sources[i];
      if (x.kind === 'service' && x.url && x.url + path === url) return x.id;
    }
    if (url.indexOf(AI_GATEWAY_BASE) === 0) {
      var id = url.slice(AI_GATEWAY_BASE.length).split('/')[0];
      if (id) {
        try {
          return 'provider:' + decodeURIComponent(id);
        } catch (e) {
          return 'provider:' + id;
        }
      }
    }
    return 'custom';
  }

  /** Legacy fallback pocket voice list (no catalog reported). */
  var FALLBACK_POCKET_VOICES = ['', 'alba', 'cosette', 'eponine', 'fantine', 'javert', 'jean', 'marius'];

  /** Defensive parse of the settings.get `ttsVoiceCatalog` side key.
   *  Absent / malformed / empty → null (legacy hardcoded UI). */
  function parseTtsVoiceCatalog(raw) {
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return null;
    var engines = raw.engines;
    if (!Array.isArray(engines)) return null;
    var out = [];
    for (var i = 0; i < engines.length; i++) {
      var e = engines[i];
      if (!e || typeof e !== 'object' || Array.isArray(e)) continue;
      if (typeof e.id !== 'string' || !e.id) continue;
      var entry = {
        id: e.id,
        label: typeof e.label === 'string' && e.label ? e.label : e.id,
      };
      if (Array.isArray(e.voices)) {
        var voices = [];
        for (var v = 0; v < e.voices.length; v++) {
          if (typeof e.voices[v] === 'string' && e.voices[v] !== '') voices.push(e.voices[v]);
        }
        entry.voices = voices;
      }
      if (Array.isArray(e.bundles)) {
        var bundles = [];
        for (var b = 0; b < e.bundles.length; b++) {
          var br = e.bundles[b];
          if (!br || typeof br !== 'object' || Array.isArray(br)) continue;
          if (typeof br.dir !== 'string' || !br.dir) continue;
          bundles.push({
            dir: br.dir,
            name: typeof br.name === 'string' && br.name ? br.name : br.dir,
            kind: typeof br.kind === 'string' && br.kind ? br.kind : 'vits',
          });
        }
        entry.bundles = bundles;
      }
      if (typeof e.scanRoot === 'string' && e.scanRoot) entry.scanRoot = e.scanRoot;
      out.push(entry);
    }
    return out.length > 0 ? out : null;
  }

  /** The pocket voice select options: '' (Default) + installed voices
   *  sorted/deduped; the legacy hardcoded list when no catalog. */
  function pocketVoiceOptions(catalog) {
    var pocket = null;
    if (catalog) {
      for (var i = 0; i < catalog.length; i++) {
        if (catalog[i].id === 'pocket') {
          pocket = catalog[i];
          break;
        }
      }
    }
    var voices = (pocket && pocket.voices) || [];
    if (voices.length === 0) return FALLBACK_POCKET_VOICES;
    var seen = {};
    var unique = [];
    for (var j = 0; j < voices.length; j++) {
      if (!seen[voices[j]]) {
        seen[voices[j]] = true;
        unique.push(voices[j]);
      }
    }
    unique.sort(function (a, b) {
      return a.localeCompare(b);
    });
    return [''].concat(unique);
  }

  var BUNDLE_ENGINE_TOKENS = { vits: 1, piper: 1, kokoro: 1, matcha: 1, mms: 1, coqui: 1, icefall: 1 };
  var BUNDLE_QUALITY_TOKENS = { low: 1, medium: 1, high: 1, x_low: 1, x_high: 1 };

  /** Human label for a sherpa voice-bundle folder name:
   *  'vits-piper-en_GB-jenny_dioco-medium' → 'Jenny Dioco (en_GB, medium)'. */
  function prettifyBundleName(name) {
    var parts = name.split('-').filter(Boolean);
    var i = 0;
    while (i < parts.length && BUNDLE_ENGINE_TOKENS[parts[i].toLowerCase()]) i += 1;
    var rest = parts.slice(i);
    if (rest.length === 0) return name;
    var locale = /^[a-z]{2,3}(_[A-Z]{2})?$/.test(rest[0]) ? rest[0] : null;
    var last = rest[rest.length - 1];
    var quality = rest.length > 1 && BUNDLE_QUALITY_TOKENS[last.toLowerCase()] ? last.toLowerCase() : null;
    var voiceTokens = rest.slice(locale ? 1 : 0, quality ? rest.length - 1 : rest.length);
    if (voiceTokens.length === 0) return name;
    var voice = voiceTokens
      .join(' ')
      .split(/[_\s]+/)
      .filter(Boolean)
      .map(function (w) {
        return w.charAt(0).toUpperCase() + w.slice(1);
      })
      .join(' ');
    var meta = [locale, quality].filter(Boolean).join(', ');
    return meta ? voice + ' (' + meta + ')' : voice;
  }

  /** Engine choices when the plugin doesn't report a catalog. */
  var DEFAULT_ENGINES = [
    { id: 'pocket', label: 'Pocket-TTS' },
    { id: 'sherpa', label: 'Sherpa (Piper/VITS/Kokoro)' },
  ];

  /** Display label for an engine option (parity-locked with the console). */
  function engineOptionLabel(engine) {
    if (engine.id === 'pocket') return 'Pocket-TTS (default)';
    if (engine.id === 'sherpa') return 'Sherpa — Piper/VITS voices (fast)';
    return engine.label;
  }

  /** Sentinel <option> value for "Custom folder…" in the bundle picker. */
  var CUSTOM_BUNDLE_DIR = '__custom__';

  /** State update for a bundle-picker selection (a bundle pick writes
   *  ttsModelDir AND ttsVoice = the bundle's folder NAME). */
  function bundleSelectionUpdate(value, bundles) {
    if (value === CUSTOM_BUNDLE_DIR) return { engine: { customDir: true } };
    if (value === '') return { engine: { modelDir: '', customDir: false }, voice: '' };
    var matched = null;
    for (var i = 0; i < bundles.length; i++) {
      if (bundles[i].dir === value) {
        matched = bundles[i];
        break;
      }
    }
    var name = matched
      ? matched.name
      : value.split(/[\\/]/).filter(Boolean).pop() || value;
    return { engine: { modelDir: value, customDir: false }, voice: name };
  }

  // ======================================================================
  // Lane metadata — ported from AokieCard.tsx (CON-301 desktop side).
  // ======================================================================

  var LANES = ['llm', 'stt', 'tts'];
  var LANE_CAPABILITY = { llm: 'chat', stt: 'transcription', tts: 'speech' };
  var LANE_DEFAULT_SERVICE = { llm: null, stt: 'aokie-stt', tts: 'aokie-tts' };
  var LANE_SETTING_KEY = { llm: 'aiEndpoint', stt: 'sttEndpoint', tts: 'ttsEndpoint' };
  var LANE_LABEL = { llm: 'LLM source', stt: 'Speech-to-text source', tts: 'Text-to-speech source' };

  var AOKIE_CODEC_OPTIONS = [
    { value: 'auto', label: 'Auto' },
    { value: 'cvsd', label: 'CVSD (8kHz)' },
    { value: 'wbs', label: 'mSBC (16kHz, wideband)' },
  ];

  /** The option list for one lane's source select (Aokie default speech
   *  service first, capability-matching local services, providers on the LLM
   *  lane only, then Custom URL… and Automatic = ''). */
  function laneSourceOptions(lane, sources, currentSource) {
    var cap = LANE_CAPABILITY[lane];
    var def = LANE_DEFAULT_SERVICE[lane];
    var services = sources.filter(function (s) {
      return s.kind === 'service' && (s.capabilities || []).indexOf(cap) !== -1;
    });
    var ordered = services
      .filter(function (s) {
        return s.serviceId === def;
      })
      .concat(
        services.filter(function (s) {
          return s.serviceId !== def;
        })
      );
    var opts = ordered.map(function (s) {
      return {
        value: s.id,
        label: 'This computer: ' + s.name + (s.status === 'running' ? '' : ' (stopped)'),
      };
    });
    if (lane === 'llm') {
      var listed = {};
      for (var i = 0; i < sources.length; i++) {
        var p = sources[i];
        if (p.kind !== 'provider') continue;
        var caps = p.capabilities || [];
        if (caps.length > 0 && caps.indexOf(cap) === -1) continue; // [] = all (legacy)
        var variant = codexVariantForSource(p.id);
        listed[p.id] = true;
        opts.push({
          value: p.id,
          // Never surface the connected account identity here. These reserved
          // reserved adapters have stable product + reasoning labels only.
          label: variant ? 'Provider: ChatGPT via Codex — ' + variant : 'Provider: ' + p.name,
        });
      }
      // Source discovery is best-effort and can briefly return no providers
      // while Desktop is starting. A saved reserved Codex endpoint is still
      // unambiguous, so keep that exact choice representable instead of
      // letting the native <select> visually fall through to its first item.
      if (codexVariantForSource(currentSource) && !listed[currentSource]) {
        opts.push({
          value: currentSource,
          label: 'Provider: ChatGPT via Codex — ' + codexVariantForSource(currentSource),
        });
      }
    }
    opts.push({ value: 'custom', label: 'Custom URL…' });
    opts.push({ value: '', label: 'Automatic (built-in)' });
    return opts;
  }

  /** Seed one lane's select from the saved endpoint URL — anything the
   *  option list can't represent degrades to 'custom' (never silently lost). */
  function seedLaneSource(saved, lane, sources) {
    var inferred = inferLaneSource(saved, lane, sources);
    if (inferred === '' || inferred === 'custom') return inferred;
    // Exact reserved URLs identify stable Desktop-owned adapters. Do not
    // degrade them to Custom merely because aiSources() raced startup. A
    // provider-id-looking near miss (for example, a trailing slash) stays
    // Custom and can never be rewritten by an unrelated save.
    if (lane === 'llm' && codexVariantForSource(inferred)) {
      return codexProviderForEndpoint(saved) ? inferred : 'custom';
    }
    var opts = laneSourceOptions(lane, sources);
    for (var i = 0; i < opts.length; i++) {
      if (opts[i].value === inferred) return inferred;
    }
    return 'custom';
  }

  // ======================================================================
  // Tab state (module-level — survives tab switches, like the compiled
  // form's component state survives collapse/expand).
  // ======================================================================

  var root = null;
  var loaded = false;
  var loading = false;
  var saving = false;
  var error = null;
  var settings = withAokieDefaults(null);
  var baseline = withAokieDefaults(null);
  var sources = [];
  var laneSel = { llm: '', stt: '', tts: '' };
  var baselineLaneSel = { llm: '', stt: '', tts: '' };
  var catalog = null;
  var customDir = false;
  // Provenance watch (live report 2026-07-18: "I changed the greeting and it
  // did not take"). A linked FormLogic app re-applies its Receptionist
  // Settings record on EVERY incoming call (the configure-receptionist flow
  // calls settings.set with persona/greeting/voice/model/endpoints), so an
  // edit made here is genuinely saved and then genuinely replaced moments
  // later. The plugin bumps configVersion on every write, so a bump this tab
  // did not cause IS an external writer — we watch for it and say so plainly
  // instead of letting the operator conclude the form is broken.
  var configVersion = null;
  var appManaged = false;

  /** Keys the linked app's configure-receptionist flow re-applies per call. */
  var APP_MANAGED_KEYS = [
    'persona',
    'greeting',
    'ttsVoice',
    'aiModel',
    'aiEndpoint',
    'sttEndpoint',
    'ttsEndpoint',
    'aiReceptionist',
  ];

  /** True once an external writer has been observed bumping configVersion. */
  function noteConfigVersion(next, ours) {
    if (typeof next !== 'number') return;
    if (configVersion !== null && next !== configVersion && !ours) {
      appManaged = true;
    }
    configVersion = next;
  }

  /** Lane picks are UI state (the settings bag stores only composed URLs), so
   *  include them when deciding whether an automatic tab-entry refresh may
   *  replace the working copy. */
  function hasUnsavedEdits() {
    if (Object.keys(settingsPatch(baseline, settings)).length > 0) return true;
    for (var i = 0; i < LANES.length; i++) {
      var lane = LANES[i];
      if (laneSel[lane] !== baselineLaneSel[lane]) return true;
    }
    return false;
  }

  function seededLanes(saved, availableSources) {
    return {
      llm: seedLaneSource(saved.aiEndpoint, 'llm', availableSources),
      stt: seedLaneSource(saved.sttEndpoint, 'stt', availableSources),
      tts: seedLaneSource(saved.ttsEndpoint, 'tts', availableSources),
    };
  }

  function load(preserveEdits) {
    if (loading) {
      render();
      return;
    }
    loading = true;
    error = null;
    render();
    var settingsP = HOST.command('settings.get');
    // Source listing is best-effort — a failure must not block the form.
    var sourcesP = HOST.aiSources().then(
      function (list) {
        return { ok: true, list: Array.isArray(list) ? list : [] };
      },
      function () {
        return { ok: false, list: [] };
      }
    );
    return Promise.all([settingsP, sourcesP]).then(
      function (results) {
        var data = results[0] || {};
        var merged = withAokieDefaults(data.settings);
        // A tab-entry refresh must not eat edits that were already present or
        // were typed while the two reads were in flight. Source metadata may
        // still refresh safely; the working settings + their old baseline stay
        // paired until the operator saves or explicitly presses Reload.
        var keepWorkingCopy = !!preserveEdits && loaded && hasUnsavedEdits();
        var sourceResult = results[1];
        if (sourceResult.ok) sources = sourceResult.list;
        // ⚠️ baseline and settings must be SEPARATE objects: the form edits
        // MUTATE `settings` in place (plain DOM, not React state-replace),
        // and an aliased baseline would make every dirty-diff empty.
        if (!keepWorkingCopy) {
          baseline = merged;
          settings = withAokieDefaults(merged);
          baselineLaneSel = seededLanes(merged, sources);
          laneSel = {
            llm: baselineLaneSel.llm,
            stt: baselineLaneSel.stt,
            tts: baselineLaneSel.tts,
          };
          customDir = false;
        }
        // Keep the shared transport truth (the Dongle tab's visibility in
        // app.js) in step with the plugin's saved settings.
        if (TABS.transport && TABS.transport.update) TABS.transport.update(merged);
        // A bump between polls that this tab did not cause = the linked app
        // re-applied its record (see the provenance note above).
        noteConfigVersion(data.configVersion, false);
        catalog = parseTtsVoiceCatalog(data.ttsVoiceCatalog);
        loaded = true;
        loading = false;
        render();
      },
      function (e) {
        loading = false;
        error = errMsg(e);
        render();
      }
    );
  }

  function save() {
    if (saving) return;
    // Endpoint lanes save the COMPOSED URL (the plugin settings bag stores
    // URLs, not source ids); a stopped/vanished service composes '' — the
    // plugin's built-in default — with an honest toast below.
    var current = {};
    var keys = Object.keys(settings);
    for (var i = 0; i < keys.length; i++) current[keys[i]] = settings[keys[i]];
    current.aiEndpoint = composeLaneUrl(laneSel.llm, settings.aiEndpoint, 'llm', sources);
    current.sttEndpoint = composeLaneUrl(laneSel.stt, settings.sttEndpoint, 'stt', sources);
    current.ttsEndpoint = composeLaneUrl(laneSel.tts, settings.ttsEndpoint, 'tts', sources);
    var selectedCodexModel = codexModelForEndpoint(current.aiEndpoint);
    if (selectedCodexModel) {
      // The reserved Codex adapters are text-only and pin the raw upstream
      // model. Mirror the connector-side invariant so the saved patch and
      // the visible form are immediately truthful.
      current.aiModel = selectedCodexModel;
      current.sendAudio = false;
      settings.aiModel = selectedCodexModel;
      settings.sendAudio = false;
    }
    var patch = settingsPatch(baseline, current);
    if (Object.keys(patch).length === 0) {
      HOST.toast('success', 'No changes to save');
      return;
    }
    saving = true;
    error = null;
    render();
    // Snapshot of the picks as they were at save time (for the stopped-pick
    // toast — laneSel is reseeded from the saved URLs after the write).
    var laneSelBefore = { llm: laneSel.llm, stt: laneSel.stt, tts: laneSel.tts };
    HOST.command('settings.set', patch)
      .then(
        function (data) {
          var merged = withAokieDefaults((data || {}).settings);
          // Separate objects — see the aliasing note in load().
          baseline = merged;
          settings = withAokieDefaults(merged);
          // The saved mode now drives the Dongle tab's visibility too.
          if (TABS.transport && TABS.transport.update) TABS.transport.update(merged);
          // OUR bump — never mistake a successful save for the linked app.
          noteConfigVersion((data || {}).configVersion, true);
          // The set response may not carry the catalog side key — keep the
          // one from the last settings.get rather than dropping to fallback.
          var cat = parseTtsVoiceCatalog((data || {}).ttsVoiceCatalog);
          if (cat) catalog = cat;
          customDir = false;
          baselineLaneSel = seededLanes(merged, sources);
          laneSel = {
            llm: baselineLaneSel.llm,
            stt: baselineLaneSel.stt,
            tts: baselineLaneSel.tts,
          };
          var stoppedNames = [];
          for (var li = 0; li < LANES.length; li++) {
            var lane = LANES[li];
            if (
              laneSelBefore[lane] &&
              laneSelBefore[lane].indexOf('service:') === 0 &&
              current[LANE_SETTING_KEY[lane]] === ''
            ) {
              var name = laneSelBefore[lane].slice(8);
              for (var si = 0; si < sources.length; si++) {
                if (sources[si].id === laneSelBefore[lane]) {
                  name = sources[si].name;
                  break;
                }
              }
              stoppedNames.push(name);
            }
          }
          if (stoppedNames.length > 0) {
            HOST.toast(
              'info',
              stoppedNames.join(', ') +
                ': saved as Automatic (built-in) — pick it again once the service is running.'
            );
          }
          var blocked = data && typeof data.blocked === 'string' ? data.blocked.trim() : '';
          if (blocked) {
            HOST.toast(
              'error',
              'Settings saved, but the receptionist is paused. Open Consent and accept the new data destination before calls can resume.'
            );
          } else {
            HOST.toast('success', 'Receptionist settings saved — takes effect on the next caller turn.');
          }
        },
        function (e) {
          error = errMsg(e);
        }
      )
      .then(function () {
        saving = false;
        render();
      });
  }

  // Number inputs: guard against NaN AND a momentarily-empty field while
  // typing so neither ever reaches state or the save payload.
  function setNumberField(key, raw) {
    if (String(raw).trim() === '') return;
    var n = Number(raw);
    if (isNaN(n)) return;
    settings[key] = n;
  }

  // ======================================================================
  // Rendering (plain DOM). The full form renders on load/save/reload;
  // the voice zone re-renders on engine/bundle changes; lane custom-URL
  // rows toggle in place — text inputs never rebuild under a keystroke.
  // ======================================================================

  function field(labelHtml, controlHtml) {
    return '<label class="rcp-field"><span>' + labelHtml + '</span>' + controlHtml + '</label>';
  }

  function hint(text, warn) {
    return '<p class="rcp-hint' + (warn ? ' is-warn' : '') + '">' + text + '</p>';
  }

  function check(key, label) {
    return (
      '<label class="rcp-check"><input type="checkbox" data-bool="' + key + '"' +
      (settings[key] ? ' checked' : '') + ' /><span>' + label + '</span></label>'
    );
  }

  function voiceZoneHtml() {
    var isSherpa = settings.ttsEngine === 'sherpa';
    var bundles = [];
    if (catalog) {
      for (var i = 0; i < catalog.length; i++) {
        if (catalog[i].id === 'sherpa' && catalog[i].bundles) bundles = catalog[i].bundles;
      }
    }
    var voiceOptions = pocketVoiceOptions(catalog);
    var unlistedVoice = settings.ttsVoice !== '' && voiceOptions.indexOf(settings.ttsVoice) === -1;
    var matchedBundle = null;
    for (var b = 0; b < bundles.length; b++) {
      if (bundles[b].dir === settings.ttsModelDir) matchedBundle = bundles[b];
    }
    // A stored folder outside the catalog renders as the Custom choice with
    // the input pre-filled — never silently discarded.
    var bundleValue =
      customDir || (settings.ttsModelDir !== '' && !matchedBundle)
        ? CUSTOM_BUNDLE_DIR
        : matchedBundle
          ? matchedBundle.dir
          : '';
    var showCustomDir = isSherpa && (bundles.length === 0 || bundleValue === CUSTOM_BUNDLE_DIR);
    var isKokoro = isSherpa && matchedBundle && matchedBundle.kind === 'kokoro';
    var html = [];

    if (!isSherpa) {
      var vopts = [];
      for (var v = 0; v < voiceOptions.length; v++) {
        var vo = voiceOptions[v];
        vopts.push(
          '<option value="' + esc(vo) + '"' + (settings.ttsVoice === vo ? ' selected' : '') + '>' +
            (vo === '' ? 'Default' : esc(vo.charAt(0).toUpperCase() + vo.slice(1))) +
            '</option>'
        );
      }
      if (unlistedVoice) {
        vopts.push('<option value="' + esc(settings.ttsVoice) + '" selected>' + esc(settings.ttsVoice) + ' (current)</option>');
      }
      html.push(field('Voice', '<select data-key="ttsVoice">' + vopts.join('') + '</select>'));
      html.push(
        hint(
          catalog
            ? 'Pocket-TTS voices installed on this machine.'
            : 'Pocket-TTS voice — the plugin reports its installed list once it runs.'
        )
      );
    }

    if (isSherpa && bundles.length > 0) {
      var bopts = ['<option value=""' + (bundleValue === '' ? ' selected' : '') + '>Automatic (first installed voice)</option>'];
      for (var bi = 0; bi < bundles.length; bi++) {
        bopts.push(
          '<option value="' + esc(bundles[bi].dir) + '"' +
            (bundleValue === bundles[bi].dir ? ' selected' : '') + '>' +
            esc(prettifyBundleName(bundles[bi].name)) +
            '</option>'
        );
      }
      bopts.push(
        '<option value="' + CUSTOM_BUNDLE_DIR + '"' + (bundleValue === CUSTOM_BUNDLE_DIR ? ' selected' : '') + '>Custom folder…</option>'
      );
      html.push(field('Voice', '<select id="set-bundle">' + bopts.join('') + '</select>'));
      html.push(
        hint(
          "Installed sherpa voice bundles — a pick sets the live voice and the in-process fallback engine's model folder together."
        )
      );
    }

    if (showCustomDir) {
      html.push(
        field(
          'Voice model folder',
          '<input type="text" data-customdir="1" placeholder="blank = first voice under models\\tts" value="' +
            esc(settings.ttsModelDir) + '" />'
        )
      );
      html.push(
        hint(
          "A sherpa voice bundle folder on this machine (the voice's .onnx + tokens.txt, e.g. vits-piper-en_US-lessac-medium)."
        )
      );
    }

    if (isKokoro) {
      html.push(
        field(
          'Speaker id',
          '<input type="text" data-key="ttsVoice" inputmode="numeric" placeholder="e.g. 0" value="' +
            esc(settings.ttsVoice) + '" />'
        )
      );
      html.push(hint('Kokoro bundles hold many speakers — the numeric id picks one.'));
    }

    return html.join('');
  }

  function laneRowsHtml(lanes) {
    lanes = lanes || LANES;
    var html = [];
    for (var i = 0; i < lanes.length; i++) {
      var lane = lanes[i];
      var key = LANE_SETTING_KEY[lane];
      var opts = laneSourceOptions(lane, sources, laneSel[lane]);
      var sel = [];
      for (var o = 0; o < opts.length; o++) {
        sel.push(
          '<option value="' + esc(opts[o].value) + '"' +
            (laneSel[lane] === opts[o].value ? ' selected' : '') + '>' +
            esc(opts[o].label) +
            '</option>'
        );
      }
      var placeholder =
        lane === 'llm'
          ? 'e.g. http://127.0.0.1:8080/v1/chat/completions'
          : 'e.g. http://127.0.0.1:17920' + (lane === 'stt' ? '/v1/audio/transcriptions' : '/v1/audio/speech');
      html.push(
        '<div>' +
          field(LANE_LABEL[lane], '<select data-lane="' + lane + '">' + sel.join('') + '</select>') +
          '<div id="lane-custom-' + lane + '"' + (laneSel[lane] === 'custom' ? '' : ' hidden') + '>' +
          field(
            'Custom URL',
            '<input type="text" data-key="' + key + '" placeholder="' + esc(placeholder) + '" value="' +
              esc(settings[key]) + '" />'
          ) +
          '</div>' +
          '</div>'
      );
    }
    return html.join('');
  }

  /** Keep the source immediately above its editable model. Codex adapters own
   *  an exact model, so they show a concise fixed-model note and no fake input. */
  function llmModelHtml() {
    var fixedModel = codexModelForSource(laneSel.llm);
    return (
      '<div id="set-llm-model-zone"' + (fixedModel ? ' hidden' : '') + '>' +
      field(
        'LLM model',
        '<input type="text" data-key="aiModel" placeholder="blank = auto-detect" value="' + esc(settings.aiModel) + '" />'
      ) +
      hint(
        'e.g. llama3.1:8b or qwen2.5:7b — leave blank to use the model currently loaded by the selected service.'
      ) +
      '</div>' +
      '<p class="rcp-hint" id="set-llm-fixed-model"' + (fixedModel ? '' : ' hidden') + '>' +
      (fixedModel
        ? 'LLM model is fixed to <strong>' + esc(fixedModel) + '</strong> by this ChatGPT via Codex source.'
        : '') +
      '</p>'
    );
  }

  function syncLlmModelUi() {
    var fixedModel = codexModelForSource(laneSel.llm);
    var zone = root && root.querySelector('#set-llm-model-zone');
    var note = root && root.querySelector('#set-llm-fixed-model');
    var input = root && root.querySelector('[data-key="aiModel"]');
    if (zone) zone.hidden = !!fixedModel;
    if (note) {
      note.hidden = !fixedModel;
      note.innerHTML = fixedModel
        ? 'LLM model is fixed to <strong>' + esc(fixedModel) + '</strong> by this ChatGPT via Codex source.'
        : '';
    }
    if (input) input.value = settings.aiModel;
  }

  function formHtml() {
    var engines = catalog && catalog.length > 0 ? catalog : DEFAULT_ENGINES;
    // Stored '' and 'pocket' both mean Pocket-TTS — the select's pocket
    // option keeps the legacy '' value so save semantics never change.
    var engineValue = settings.ttsEngine === 'pocket' ? '' : settings.ttsEngine;
    var engineOpts = [];
    var engineListed = false;
    for (var i = 0; i < engines.length; i++) {
      var val = engines[i].id === 'pocket' ? '' : engines[i].id;
      if (val === engineValue) engineListed = true;
      engineOpts.push(
        '<option value="' + esc(val) + '"' + (engineValue === val ? ' selected' : '') + '>' +
          esc(engineOptionLabel(engines[i])) +
          '</option>'
      );
    }
    if (!engineListed) {
      engineOpts.push('<option value="' + esc(engineValue) + '" selected>' + esc(engineValue) + '</option>');
    }

    var codecOpts = [];
    for (var c = 0; c < AOKIE_CODEC_OPTIONS.length; c++) {
      var co = AOKIE_CODEC_OPTIONS[c];
      codecOpts.push(
        '<option value="' + co.value + '"' + (settings.hfpCodec === co.value ? ' selected' : '') + '>' +
          esc(co.label) +
          '</option>'
      );
    }

    return (
      '<form class="rcp-form" id="set-form">' +
      // ---- AI receptionist -------------------------------------------------
      '<div>' +
      '<h4 class="rcp-group-title">AI receptionist</h4>' +
      check('aiReceptionist', 'AI receptionist replies live') +
      hint(
        'When on, the plugin answers callers itself in real time (speech-to-text → local LLM → text-to-speech). When off, a FormLogic flow must speak for it.'
      ) +
      check('autoAnswer', 'Auto-answer incoming calls') +
      '</div>' +
      // ---- Persona & voice -------------------------------------------------
      '<div>' +
      '<h4 class="rcp-group-title">Persona &amp; voice</h4>' +
      field(
        'Greeting (spoken first)',
        '<input type="text" data-key="greeting" placeholder="Thanks for calling! How can I help you today?" value="' +
          esc(settings.greeting) + '" />'
      ) +
      hint('Blank = a friendly built-in default.') +
      field(
        'Persona / instructions',
        '<textarea rows="4" data-key="persona" placeholder="e.g. Be warm and concise. Offer to book Mon–Fri 9–5.">' +
          esc(settings.persona) + '</textarea>'
      ) +
      hint(
        "Blank = the built-in receptionist script (greet, ask the caller's name and reason, capture details, book or take a message)."
      ) +
      field('Speech engine', '<select id="set-engine">' + engineOpts.join('') + '</select>') +
      hint(
        'Applies live. Sherpa speaks Piper/VITS/Kokoro voice bundles (much faster than Pocket-TTS); each engine has its own voice list below.'
      ) +
      '<div id="set-voice-zone">' + voiceZoneHtml() + '</div>' +
      laneRowsHtml(['llm']) +
      llmModelHtml() +
      laneRowsHtml(['stt', 'tts']) +
      hint(
        'Composed from the selected service now; if the FormLogic receptionist app is connected, its per-call settings take precedence.'
      ) +
      hint(
        'ChatGPT via Codex choices send transcript text to OpenAI under the signed destination consent. GPT-5.6 Luna uses low reasoning, its fastest supported setting, and streams its reply sentence by sentence. Fast mode requests Codex priority service, though actual latency still varies. These choices are text-only: the selected model is fixed automatically and caller-audio attachment is disabled.'
      ) +
      '</div>' +
      // ---- Conversation tuning ---------------------------------------------
      '<div>' +
      '<h4 class="rcp-group-title">Conversation tuning</h4>' +
      field(
        'Reply delay (ms)',
        '<input type="number" data-num="sttEndpointMs" min="150" max="2000" step="50" value="' +
          esc(settings.sttEndpointMs) + '" />'
      ) +
      hint(
        'How long the caller must pause before Aokie treats their turn as finished. Lower = snappier, but risks cutting off mid-sentence pauses.'
      ) +
      check('bargeIn', 'Full-duplex (barge-in)') +
      hint('Let the caller talk over Aokie — it stops the instant they speak, using echo cancellation.') +
      field(
        'Barge-in sensitivity',
        '<input type="number" data-num="bargeSensitivity" min="100" max="2000" step="25" value="' +
          esc(settings.bargeSensitivity) + '" />'
      ) +
      hint('Lower = easier to interrupt. Only used when barge-in is on.') +
      '</div>' +
      // ---- Advanced --------------------------------------------------------
      '<div>' +
      '<h4 class="rcp-group-title">Advanced</h4>' +
      // Phone connection: the Aokie USB dongle is the only transport offered.
      // The native Windows-Bluetooth backend (transportMode setting) exists
      // for advanced use, but Windows 11 25H2 removed the OS hands-free
      // service — native mode cannot carry call audio there, so the mode is
      // pinned to the dongle in the UI rather than offered as a choice that
      // silently breaks calls.
      hint('Phone connection: Aokie USB dongle.') +
      // Dongle-only hardware knobs — meaningless on the native Windows
      // Bluetooth transport, so they hide (in place, order preserved) unless
      // the mode is dongle. Each keeps its own wrapper so answerTone and the
      // surrounding layout render exactly as before in dongle mode.
      '<div class="set-dongle-only"' + (settings.transportMode === 'dongle' ? '' : ' hidden') + '>' +
      field('Bluetooth audio codec', '<select data-key="hfpCodec">' + codecOpts.join('') + '</select>') +
      hint('Dongle mode only. Some dongles only work reliably with CVSD; mSBC gives better speech-recognition accuracy where supported.') +
      '</div>' +
      check('answerTone', 'Play a test tone on answer') +
      hint('Diagnostic: verifies the outbound audio path reaches the caller. Leave off for normal use.') +
      '<div class="set-dongle-only"' + (settings.transportMode === 'dongle' ? '' : ' hidden') + '>' +
      field(
        'Re-enumerate hardware id on start',
        '<input type="text" data-key="reenumerateHwid" placeholder="e.g. USB\\VID_0A5C&amp;PID_21EC" value="' +
          esc(settings.reenumerateHwid) + '" />'
      ) +
      hint(
        "Workaround for dongles whose audio is dead after a cold boot until replugged. Leave blank unless you've hit that issue."
      ) +
      '</div>' +
      '<div class="set-dongle-only"' + (settings.transportMode === 'dongle' ? '' : ' hidden') + '>' +
      check('legacyPairingPin', 'Allow legacy PIN pairing (compatibility)') +
      hint(
        "⚠️ Uses the fixed PIN 0000 for very old devices that can't do modern code-confirmation pairing — it provides no protection against a nearby impostor. Enable only while pairing such a device, then turn it back off.",
        true
      ) +
      '</div>' +
      '</div>' +
      // ---- Actions ---------------------------------------------------------
      // ⚠️ NOT type="submit": the sandboxed iframe (allow-scripts only, CSP
      // form-action 'none') BLOCKS native form submission BEFORE the submit
      // event fires — a submit button would be dead. Save rides the click
      // delegate; Enter-to-save rides the keydown handler in wire().
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button is-primary" id="set-save" data-act="set-save"' + (saving || loading ? ' disabled' : '') + '>' +
      (saving ? 'Saving…' : 'Save') +
      '</button>' +
      '<button type="button" class="rcp-button" data-act="set-reload"' + (loading || saving ? ' disabled' : '') + '>Reload</button>' +
      '</div>' +
      '</form>'
    );
  }

  function render() {
    if (!root) return;
    var body;
    if (loading && !loaded) {
      body = '<p class="rcp-loading">Loading…</p>';
    } else if (!loaded) {
      body =
        '<div class="rcp-card__body"><p class="rcp-inline-note">Couldn\'t load receptionist settings.' +
        (error ? ' ' + esc(error) : '') +
        '</p><div class="rcp-actions"><button type="button" class="rcp-button" data-act="set-retry">Retry</button></div></div>';
    } else {
      body = formHtml();
    }
    root.innerHTML =
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>CONFIGURE RECEPTIONIST</small>' +
      '<h3>Receptionist settings</h3>' +
      '</div>' +
      '</div>' +
      appManagedNoticeHtml() +
      body +
      (loaded && error
        ? '<p class="rcp-error">' + esc(error) + '</p>'
        : '') +
      '</section>';
  }

  /**
   * The honest answer to "I changed the greeting and it did not take".
   *
   * A linked FormLogic app re-applies its Receptionist Settings record on
   * every incoming call, so edits to the app-managed keys here are saved and
   * then replaced. Shown as a warning ONCE an external configVersion bump has
   * actually been observed (so a standalone Aokie, where this form IS the
   * source of truth, never nags); a quieter always-on line states the
   * relationship up front.
   */
  function appManagedNoticeHtml() {
    if (!loaded) return '';
    if (appManaged) {
      return (
        '<p class="rcp-notice rcp-notice--warn" role="status">' +
        '<strong>Your FormLogic app just re-applied these settings' +
        (typeof configVersion === 'number' ? ' (config v' + configVersion + ')' : '') +
        '.</strong> ' +
        'It does that on every incoming call, so greeting, persona, voice, model and endpoints ' +
        'edited here are replaced by the app&rsquo;s Receptionist Settings record. ' +
        'Edit them in the app to make them stick — everything else on this page ' +
        '(call handling, audio, screening, hardware) is owned here.' +
        '</p>'
      );
    }
    return (
      '<p class="rcp-notice" role="note">' +
      'If this receptionist is linked to a FormLogic app, that app re-applies ' +
      'greeting, persona, voice, model and endpoints on every incoming call — ' +
      'edit those in the app&rsquo;s Receptionist Settings. The rest of this page is owned here.' +
      '</p>'
    );
  }

  function rerenderVoiceZone() {
    var zone = root && root.querySelector('#set-voice-zone');
    if (zone) zone.innerHTML = voiceZoneHtml();
  }

  // ---- event wiring (delegated once per container) ------------------------

  function onInputOrChange(e) {
    var t = e.target;
    if (!t || !t.getAttribute) return;

    var key = t.getAttribute('data-key');
    if (key != null) {
      settings[key] = t.value;
      if (key === 'transportMode') {
        // Live feedback for the working copy: the dongle-only Advanced
        // fields toggle in place (same pattern as the lane custom-URL rows).
        // The Dongle tab itself follows the SAVED mode — it flips when this
        // form saves and the plugin's settings come back.
        var hideDongleFields = t.value !== 'dongle';
        var zones = root ? root.querySelectorAll('.set-dongle-only') : [];
        for (var zi = 0; zi < zones.length; zi++) zones[zi].hidden = hideDongleFields;
      }
      return;
    }
    var num = t.getAttribute('data-num');
    if (num != null) {
      if (e.type === 'blur' || e.type === 'focusout') {
        t.value = String(settings[num]);
        return;
      }
      setNumberField(num, t.value);
      return;
    }
    if (t.getAttribute('data-customdir') != null) {
      customDir = true;
      settings.ttsModelDir = t.value;
      return;
    }
    var boolKey = t.getAttribute('data-bool');
    if (boolKey != null && e.type === 'change') {
      settings[boolKey] = !!t.checked;
      return;
    }
    var lane = t.getAttribute('data-lane');
    if (lane != null && e.type === 'change') {
      laneSel[lane] = t.value;
      var row = root && root.querySelector('#lane-custom-' + lane);
      if (row) row.hidden = laneSel[lane] !== 'custom';
      var selectedSourceModel = lane === 'llm' ? codexModelForSource(laneSel.llm) : null;
      if (selectedSourceModel) {
        settings.aiModel = selectedSourceModel;
        settings.sendAudio = false;
      }
      if (lane === 'llm') syncLlmModelUi();
      return;
    }
    if (t.id === 'set-engine' && e.type === 'change') {
      settings.ttsEngine = t.value;
      rerenderVoiceZone();
      return;
    }
    if (t.id === 'set-bundle' && e.type === 'change') {
      var bundles = [];
      if (catalog) {
        for (var i = 0; i < catalog.length; i++) {
          if (catalog[i].id === 'sherpa' && catalog[i].bundles) bundles = catalog[i].bundles;
        }
      }
      var u = bundleSelectionUpdate(t.value, bundles);
      customDir = !!u.engine.customDir;
      if (u.engine.modelDir !== undefined) settings.ttsModelDir = u.engine.modelDir;
      if (u.voice !== undefined) settings.ttsVoice = u.voice;
      rerenderVoiceZone();
    }
  }

  function wire(el) {
    if (el.__aokieSettingsWired) return;
    el.__aokieSettingsWired = true;
    el.addEventListener('input', onInputOrChange);
    el.addEventListener('change', onInputOrChange);
    el.addEventListener('focusout', onInputOrChange);
    // Defensive only: in the production sandbox (allow-scripts, CSP
    // form-action 'none') a native submission is blocked BEFORE this event
    // would fire — Save is driven by the click/keydown handlers below.
    el.addEventListener('submit', function (e) {
      var form = e.target;
      if (form && form.id === 'set-form') {
        e.preventDefault();
        save();
      }
    });
    // Enter in a form input = save (the implicit-submission affordance the
    // compiled form had; the sandbox blocks the native path silently).
    el.addEventListener('keydown', function (e) {
      if (e.key !== 'Enter') return;
      var t = e.target;
      if (!t || t.tagName !== 'INPUT') return;
      if (!t.closest || !t.closest('#set-form')) return;
      e.preventDefault();
      if (!saving && !loading) save();
    });
    el.addEventListener('click', function (e) {
      var btn = e.target && e.target.closest ? e.target.closest('[data-act]') : null;
      if (!btn || btn.disabled) return;
      var act = btn.getAttribute('data-act');
      if (act === 'set-save') save();
      else if (act === 'set-reload' || act === 'set-retry') load(false);
    });
  }

  TABS.register('settings', {
    mount: function (el) {
      root = el;
      wire(el);
      if (loaded) {
        render();
        // The linked app and provider runtime can both change while another
        // tab is open. Refresh on every return; preserve a local draft if one
        // exists, and never turn this read path into an implicit save.
        load(true);
      } else {
        load(false);
      }
    },
    unmount: function () {
      root = null;
    },
  });
})();
