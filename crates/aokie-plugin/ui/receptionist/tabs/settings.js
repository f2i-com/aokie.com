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
    bargeSensitivity: 650,
    hfpCodec: 'auto',
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
      bargeSensitivity:
        typeof src.bargeSensitivity === 'number' && isFinite(src.bargeSensitivity)
          ? src.bargeSensitivity
          : d.bargeSensitivity,
      hfpCodec: codec === 'cvsd' || codec === 'wbs' || codec === 'auto' ? codec : d.hfpCodec,
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
  function laneSourceOptions(lane, sources) {
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
      for (var i = 0; i < sources.length; i++) {
        var p = sources[i];
        if (p.kind !== 'provider') continue;
        var caps = p.capabilities || [];
        if (caps.length > 0 && caps.indexOf(cap) === -1) continue; // [] = all (legacy)
        opts.push({ value: p.id, label: 'Provider: ' + p.name });
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

  function load() {
    loading = true;
    error = null;
    render();
    var settingsP = HOST.command('settings.get');
    // Source listing is best-effort — a failure must not block the form.
    var sourcesP = HOST.aiSources().then(
      function (list) {
        return Array.isArray(list) ? list : [];
      },
      function () {
        return [];
      }
    );
    return Promise.all([settingsP, sourcesP]).then(
      function (results) {
        var data = results[0] || {};
        var merged = withAokieDefaults(data.settings);
        // ⚠️ baseline and settings must be SEPARATE objects: the form edits
        // MUTATE `settings` in place (plain DOM, not React state-replace),
        // and an aliased baseline would make every dirty-diff empty.
        baseline = merged;
        settings = withAokieDefaults(merged);
        sources = results[1];
        // A bump between polls that this tab did not cause = the linked app
        // re-applied its record (see the provenance note above).
        noteConfigVersion(data.configVersion, false);
        catalog = parseTtsVoiceCatalog(data.ttsVoiceCatalog);
        customDir = false;
        laneSel = {
          llm: seedLaneSource(merged.aiEndpoint, 'llm', sources),
          stt: seedLaneSource(merged.sttEndpoint, 'stt', sources),
          tts: seedLaneSource(merged.ttsEndpoint, 'tts', sources),
        };
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
          // OUR bump — never mistake a successful save for the linked app.
          noteConfigVersion((data || {}).configVersion, true);
          // The set response may not carry the catalog side key — keep the
          // one from the last settings.get rather than dropping to fallback.
          var cat = parseTtsVoiceCatalog((data || {}).ttsVoiceCatalog);
          if (cat) catalog = cat;
          customDir = false;
          laneSel = {
            llm: seedLaneSource(merged.aiEndpoint, 'llm', sources),
            stt: seedLaneSource(merged.sttEndpoint, 'stt', sources),
            tts: seedLaneSource(merged.ttsEndpoint, 'tts', sources),
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
          HOST.toast('success', 'Receptionist settings saved — takes effect on the next caller turn.');
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

  function laneRowsHtml() {
    var html = [];
    for (var i = 0; i < LANES.length; i++) {
      var lane = LANES[i];
      var key = LANE_SETTING_KEY[lane];
      var opts = laneSourceOptions(lane, sources);
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
      field(
        'LLM model',
        '<input type="text" data-key="aiModel" placeholder="blank = auto-detect" value="' + esc(settings.aiModel) + '" />'
      ) +
      hint(
        "e.g. llama3.1:8b or qwen2.5:7b — leave blank to use whatever the desktop's running LLM service has loaded."
      ) +
      laneRowsHtml() +
      hint(
        'Composed from the selected service now; if the FormLogic receptionist app is connected, its per-call settings take precedence.'
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
      field('Bluetooth audio codec', '<select data-key="hfpCodec">' + codecOpts.join('') + '</select>') +
      hint('Some dongles only work reliably with CVSD; mSBC gives better speech-recognition accuracy where supported.') +
      check('answerTone', 'Play a test tone on answer') +
      hint('Diagnostic: verifies the outbound audio path reaches the caller. Leave off for normal use.') +
      field(
        'Re-enumerate hardware id on start',
        '<input type="text" data-key="reenumerateHwid" placeholder="e.g. USB\\VID_0A5C&amp;PID_21EC" value="' +
          esc(settings.reenumerateHwid) + '" />'
      ) +
      hint(
        "Workaround for dongles whose audio is dead after a cold boot until replugged. Leave blank unless you've hit that issue."
      ) +
      check('legacyPairingPin', 'Allow legacy PIN pairing (compatibility)') +
      hint(
        "⚠️ Uses the fixed PIN 0000 for very old devices that can't do modern code-confirmation pairing — it provides no protection against a nearby impostor. Enable only while pairing such a device, then turn it back off.",
        true
      ) +
      '</div>' +
      // ---- Actions ---------------------------------------------------------
      // ⚠️ NOT type="submit": the sandboxed iframe (allow-scripts only, CSP
      // form-action 'none') BLOCKS native form submission BEFORE the submit
      // event fires — a submit button would be dead. Save rides the click
      // delegate; Enter-to-save rides the keydown handler in wire().
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button is-primary" id="set-save" data-act="set-save"' + (saving ? ' disabled' : '') + '>' +
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
      else if (act === 'set-reload' || act === 'set-retry') load();
    });
  }

  TABS.register('settings', {
    mount: function (el) {
      root = el;
      wire(el);
      if (loaded) {
        render();
      } else {
        load();
      }
    },
    unmount: function () {
      root = null;
    },
  });
})();
