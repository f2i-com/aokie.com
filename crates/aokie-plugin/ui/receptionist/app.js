/*
 * Aokie AI Receptionist — plugin-shipped desktop console (manifest v2
 * ui.screens). CORE module: boot, shared helpers, the tab registry/switcher,
 * the declared-events subscription, and the Overview tab.
 *
 * Runs inside OAIY Desktop's sandboxed plugin-screen iframe. The host
 * injects `window.PluginHost` (postMessage RPC) before this file executes;
 * there is NO network, NO framework and NO build step here — plain DOM.
 *
 * The host runs every .js file in manifest files-list order, one <script>
 * per file — this file runs FIRST and defines `window.__aokieTabs`
 * (a registration API); each tabs/*.js file is an IIFE that registers
 * itself. This file mounts/unmounts tabs on switch and forwards subscribed
 * plugin events to the active tab.
 *
 * Overview structure: one `state` object, per-card render helpers, two
 * visibility-aware timers (5 s readiness/roster, 2 s live call), and event
 * delegation for the dynamic buttons. Every card paints "Loading…" first
 * and then either data or an honest inline error — never a blank card.
 */
(function () {
  'use strict';

  var HOST = window.PluginHost;
  if (!HOST) {
    document.body.innerHTML =
      '<p class="rcp-error">Open this screen in OAIY Desktop to connect to the Aokie plugin.</p>';
    return;
  }

  // ---- tiny helpers -------------------------------------------------------

  function $(id) {
    return document.getElementById(id);
  }

  /** HTML-escape a value for interpolation into innerHTML strings. */
  function esc(v) {
    return String(v == null ? '' : v).replace(/[&<>"']/g, function (c) {
      if (c === '&') return '&amp;';
      if (c === '<') return '&lt;';
      if (c === '>') return '&gt;';
      if (c === '"') return '&quot;';
      return '&#39;';
    });
  }

  /** What errMsg says when Aokie itself is not running. Cards show it as a
   *  quiet note, not an error: OAIY says so above the screen, with Start. */
  var NOT_RUNNING = "Aokie is not running, so this can't be read yet. Start it and this fills in by itself.";

  /** The screen is a page of OAIY Desktop (its PluginHost reads OAIY's own
   *  state, which FormLogic Desktop's does not): words that name FormLogic
   *  are said as they are inside OAIY. */
  var OAIY_HOST = typeof HOST.oaiyStatus === 'function';

  /** The plugin's words, as they read inside OAIY: a reason an older Aokie
   *  kept sends the person to "the FormLogic consent wizard", which is this
   *  screen's Consent tab there. */
  function hostWords(text) {
    var s = String(text == null ? '' : text);
    return OAIY_HOST ? s.replace(/re-run the FormLogic consent wizard/gi, 'review consent in the Consent tab') : s;
  }

  function errMsg(e) {
    var m = hostWords(e && e.message ? e.message : String(e));
    // The host's words when Aokie itself is not running ("503: The "aokie"
    // connector is not available: its plugin is … Not started yet …"): OAIY
    // already says so above the screen, with a Start button, so say it once
    // and plainly here rather than repeat the status code and its internals.
    if (/connector is not available|not started yet/i.test(m)) {
      return NOT_RUNNING;
    }
    // A plugin refusal leads with the command it refused ("settings.get: …",
    // "dongle.reset refused: …"): the person did not type a command, so the
    // reason alone, as a sentence.
    var lead = /^[a-z]+(?:\.[A-Za-z]+)+(?: refused)?:\s+(\S)/.exec(m);
    if (lead) m = lead[1].toUpperCase() + m.slice(lead[0].length);
    return m;
  }

  /** A card's line for a failed read: the error, or the quiet note when
   *  Aokie is not running (the same words on every card read as alarm). */
  function problemHtml(msg) {
    if (msg === NOT_RUNNING) return '<p class="rcp-empty">Shown once Aokie is running.</p>';
    return '<p class="rcp-error">' + esc(msg) + '</p>';
  }

  /** Grey bars while a card's first read is out. */
  var LOADING_HTML =
    '<div class="rcp-skeleton" role="status"><span class="rcp-sr">Loading</span><i></i><i></i><i></i></div>';

  /** Times in plugin reasons ("consent expired at 2026-10-01T09:12:00Z") in
   *  the person's own way of writing them. */
  function plainTimes(text) {
    return String(text == null ? '' : text).replace(
      /\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2})/g,
      function (iso) {
        var t = new Date(iso);
        if (isNaN(t.getTime())) return iso;
        return t.toLocaleString(undefined, { day: 'numeric', month: 'short', year: 'numeric', hour: 'numeric', minute: '2-digit' });
      }
    );
  }

  /** A reason as a sentence of its own: times made plain, a capital first. */
  function sentence(text) {
    var s = plainTimes(hostWords(text)).trim();
    return s ? s.charAt(0).toUpperCase() + s.slice(1) : s;
  }

  // ---- the phone line not running ------------------------------------------
  // Without its radio (no dongle, a dongle another program holds, or consent
  // holding it back) Aokie refuses every call and phone command with a reason
  // for developers ("call.current: the radio is not running (…) — command not
  // performed"; "dongle.diagnostics is not yet wired to hardware (outbox: …)").
  // The screens read that as one plain state, said once, with where to put it
  // right, instead of showing each refusal in red.

  /** The reason in a refusal for want of the radio ('' when it gives none),
   *  or null for any other error. */
  function radioDownReason(message) {
    var m = String(message == null ? '' : message);
    var running = /the radio is not running \((.*)\)\s*[\u2014-]+\s*command not performed/i.exec(m);
    if (running) return running[1];
    if (/not yet wired to hardware/i.test(m)) return '';
    return null;
  }

  /** What phone.status says while the radio is not running (`source`
   *  'config', its `error` why): { consent, reason }, or null while the radio
   *  runs or no reason is known. `consent`: consent holds the line back. */
  function lineDownFromPhone(status) {
    if (!status || status.source !== 'config' || !status.error) return null;
    return lineDownFor(String(status.error));
  }

  /** A not-running reason as { consent, reason }. */
  function lineDownFor(reason) {
    var consent = /^consent required:\s*/i.exec(reason);
    return consent ? { consent: true, reason: reason.slice(consent[0].length) } : { consent: false, reason: reason };
  }

  // ---- focus across a redraw ------------------------------------------------
  // A tab drawn again from its markup (a form after a save or a reload) loses
  // the field the person was in, and with it what they type next. These note
  // the focused control (by its data-* name or id), its caret and its scroll,
  // and put them back on the new one.

  var FOCUS_KEYS = ['data-key', 'data-bool', 'data-num', 'data-scope', 'data-act', 'id'];

  /** Where focus is inside `root`, or null when it is not there. */
  function focusOf(root) {
    var a = document.activeElement;
    if (!root || !a || a === root || a === document.body || !root.contains || !root.contains(a)) return null;
    for (var i = 0; i < FOCUS_KEYS.length; i++) {
      var v = a.getAttribute && a.getAttribute(FOCUS_KEYS[i]);
      if (!v) continue;
      var at = { sel: '[' + FOCUS_KEYS[i] + '="' + String(v).replace(/["\\]/g, '\\$&') + '"]' };
      try {
        if (typeof a.selectionStart === 'number') {
          at.start = a.selectionStart;
          at.end = a.selectionEnd;
          at.dir = a.selectionDirection;
        }
      } catch (e) {
        /* a number field has no caret to read */
      }
      if (typeof a.scrollTop === 'number' && a.scrollTop) at.scroll = a.scrollTop;
      return at;
    }
    return null;
  }

  /** Put focus back where `focusOf` found it. True when it took. */
  function restoreFocus(root, at) {
    if (!root || !at || !root.querySelector) return false;
    var el = root.querySelector(at.sel);
    if (!el || el.disabled || typeof el.focus !== 'function') return false;
    try {
      el.focus({ preventScroll: true });
    } catch (e) {
      el.focus();
    }
    if (typeof at.start === 'number') {
      try {
        el.setSelectionRange(at.start, at.end, at.dir || 'none');
      } catch (e) {
        /* not a text field */
      }
    }
    if (typeof at.scroll === 'number') el.scrollTop = at.scroll;
    return document.activeElement === el;
  }

  /** Nowhere in particular has focus (a redraw dropped it, or none was
   *  set): or only `root` itself, the tab's panel, which WebKit focuses for
   *  a mouse press on a button inside it (it does not focus buttons). */
  function focusLost(root) {
    var a = document.activeElement;
    return !a || a === document.body || a === document.documentElement || (!!root && a === root);
  }

  /** innerHTML with a change guard so a poll re-render never resets hover
   *  states or rebuilds identical DOM every tick. */
  function setHtml(el, html) {
    if (el.__rcpHtml === html) return;
    el.__rcpHtml = html;
    el.innerHTML = html;
  }

  function setPill(pillEl, textEl, cls, text) {
    pillEl.className = 'rcp-pill ' + cls;
    pillEl.hidden = false;
    textEl.textContent = text;
  }

  // ---- inline icons (CSP forbids external assets) -------------------------

  function svg(inner, size) {
    var s = size || 15;
    return (
      '<svg width="' + s + '" height="' + s + '" viewBox="0 0 24 24" fill="none" ' +
      'stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
      inner +
      '</svg>'
    );
  }

  var ICONS = {
    smartphone: svg('<rect width="14" height="20" x="5" y="2" rx="2" ry="2"/><path d="M12 18h.01"/>'),
    server: svg('<rect width="20" height="8" x="2" y="2" rx="2" ry="2"/><rect width="20" height="8" x="2" y="14" rx="2" ry="2"/><path d="M6 6h.01"/><path d="M6 18h.01"/>'),
    radio: svg('<circle cx="12" cy="12" r="2"/><path d="M4.9 19.1C1 15.2 1 8.8 4.9 4.9"/><path d="M7.8 16.2c-2.3-2.3-2.3-6.1 0-8.5"/><path d="M16.2 7.8c2.3 2.3 2.3 6.1 0 8.5"/><path d="M19.1 4.9C23 8.8 23 15.2 19.1 19.1"/>'),
    cloud: svg('<path d="M17.5 19H9a7 7 0 1 1 6.71-9h1.79a4.5 4.5 0 1 1 0 9Z"/>'),
    phone: svg('<path d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72c.13.96.36 1.9.7 2.81a2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45c.91.34 1.85.57 2.81.7A2 2 0 0 1 22 16.92z"/>'),
    phoneBig: svg('<path d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72c.13.96.36 1.9.7 2.81a2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45c.91.34 1.85.57 2.81.7A2 2 0 0 1 22 16.92z"/>', 22),
    x: svg('<path d="M18 6 6 18"/><path d="m6 6 12 12"/>'),
    check: svg('<circle cx="12" cy="12" r="10"/><path d="m9 12 2 2 4-4"/>', 14),
    alert: svg('<path d="m21.73 18-8-14a2 2 0 0 0-3.46 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Z"/><path d="M12 9v4"/><path d="M12 17h.01"/>', 14),
  };

  // ---- where calls go -----------------------------------------------------
  // Calls reach OAIY when the plugin streams them to OAIY Desktop's own voice
  // gateway: Desktop realtime mode with the provider "oaiy". There OAIY's
  // Front desk agent writes the replies and OAIY Voice hears and speaks, so
  // Aokie's own speech engines, model and voice lists play no part.

  var OAIY_PROVIDER_ID = 'oaiy';
  var OAIY_REALTIME_ENDPOINT = 'ws://127.0.0.1:17872/api/ai/providers/oaiy/v1/realtime/stream';
  var OAIY_DESTINATION = 'https://oaiy.localhost';

  /** The provider id in a Desktop realtime route, '' for anything else. */
  function realtimeProviderId(endpoint) {
    try {
      var seg = new URL(String(endpoint || '').trim()).pathname.split('/');
      if (
        seg.length === 8 && seg[1] === 'api' && seg[2] === 'ai' && seg[3] === 'providers' &&
        seg[5] === 'v1' && seg[6] === 'realtime' && seg[7] === 'stream'
      ) return decodeURIComponent(seg[4]);
    } catch (e) { /* not a URL */ }
    return '';
  }

  /** Where the saved settings send calls:
   *  'oaiy'     — OAIY's voice gateway (the Front desk agent answers);
   *  'realtime' — another Desktop realtime provider;
   *  'local'    — Aokie's older in-plugin receptionist, which has no speech
   *               now: nobody answers, calls ring through;
   *  'flows'    — no AI receptionist: nobody answers either (flows and the
   *               operator spoke with Aokie's own voice, which is gone);
   *  'unknown'  — settings not loaded. */
  function callRoute(bag) {
    if (!bag || typeof bag !== 'object') return 'unknown';
    if (bag.realtimeVoiceMode === 'desktop_realtime') {
      return realtimeProviderId(bag.realtimeVoiceEndpoint) === OAIY_PROVIDER_ID ? 'oaiy' : 'realtime';
    }
    if (bag.aiReceptionist === true || bag.aiReceptionist === 'true') return 'local';
    if (bag.aiReceptionist === false || bag.aiReceptionist === 'false') return 'flows';
    return 'unknown';
  }

  /** OAIY Voice (speech to text and speech) as the host's source list reports
   *  it: { status, url } or null when the list has no such service. */
  function oaiyVoiceService(sources) {
    var list = Array.isArray(sources) ? sources : [];
    for (var i = 0; i < list.length; i++) {
      if (list[i] && list[i].kind === 'service' && list[i].serviceId === 'oaiy-voice') {
        return { status: String(list[i].status || 'unknown'), url: list[i].url || '' };
      }
    }
    return null;
  }

  /** The voice chosen for calls and the engines' language model, from the
   *  host (PluginHost.oaiyStatus, OAIY Desktop 52a5a6a and later). Resolves
   *  null on an older host or a failed read, so callers show a pointer. */
  function readOaiyStatus() {
    if (typeof HOST.oaiyStatus !== 'function') return Promise.resolve(null);
    return HOST.oaiyStatus().then(
      function (status) {
        return status && typeof status === 'object' ? status : null;
      },
      function () {
        return null;
      }
    );
  }

  /** "Qwen… · ready" for the engines' model, '' when unknown. */
  function modelText(status) {
    var llm = status && status.llm;
    if (!llm) return '';
    if (!llm.running) return 'engines stopped';
    if (!llm.resident) return llm.state ? 'no model loaded · ' + llm.state : '';
    return llm.resident + (llm.state ? ' · ' + llm.state : '');
  }

  /** The chosen call voice's name, '' when unknown. */
  function voiceText(status) {
    return (status && status.voice && status.voice.chosen) || '';
  }

  /** Said wherever a route or a setting would need Aokie's own speech,
   *  which it does not have: OAIY Voice hears and speaks for calls. */
  var NO_OWN_SPEECH = 'Aokie has no speech of its own: OAIY Voice hears and speaks for calls that go to OAIY';

  var OAIY = {
    providerId: OAIY_PROVIDER_ID,
    endpoint: OAIY_REALTIME_ENDPOINT,
    destination: OAIY_DESTINATION,
    /** This screen runs inside OAIY Desktop (see OAIY_HOST). */
    isHost: OAIY_HOST,
    realtimeProviderId: realtimeProviderId,
    callRoute: callRoute,
    voiceService: oaiyVoiceService,
    readStatus: readOaiyStatus,
    modelText: modelText,
    voiceText: voiceText,
    noOwnSpeech: NO_OWN_SPEECH,
  };

  // ---- links to the dashboard's own pages ---------------------------------
  // The frame cannot navigate itself; the host opens its pages for it. On a
  // host without PluginHost.navigate the same markup reads as plain text.

  var canNavigate = typeof HOST.navigate === 'function';
  if (!canNavigate) document.documentElement.classList.add('rcp-no-nav');

  /** A link to a dashboard page ('agent', 'calendar', 'engines', ...). */
  function navLink(view, text) {
    return (
      '<button type="button" class="rcp-link-btn" data-nav="' + esc(view) + '"' +
      (canNavigate ? '' : ' tabindex="-1"') + '>' + esc(text) + '</button>'
    );
  }

  // ---- tab registry -------------------------------------------------------
  // Each tabs/*.js module registers { mount(el), unmount()?, onEvent(frame)? }.
  // The Overview tab is owned by THIS file (static markup + the timers below);
  // switching mounts/unmounts the registered module for every other tab.
  // Tab state is in-memory only (the sandbox has no location.hash).

  var tabRegistry = {};
  var activeTab = 'overview';
  var activeDef = null; // the mounted module for a non-overview tab

  // ---- setup mode (OAIY's setup wizard) -----------------------------------
  // OAIY's setup wizard can show this screen as one of its steps. It then
  // adds PluginHost.setup (absent in normal mode and on older hosts), whose
  // context() names the view to show. The screen then shows only that tab:
  // no tab bar, no Overview, no timers of its own; the wizard draws the
  // title, the step list and Back/Next/Skip around the frame. The tab keeps
  // its own inner flow and tells the wizard when the step is done.
  //
  //   'off'     — the normal screen (no PluginHost.setup: exactly as before);
  //   'pending' — PluginHost.setup is there, waiting for its context;
  //   'on'      — one tab, inside the wizard.

  var SETUP = HOST.setup && typeof HOST.setup.context === 'function' ? HOST.setup : null;
  var setupMode = SETUP ? 'pending' : 'off';
  var setupStep = '';
  var setupView = '';

  /** The wizard's view names, and the tab each one shows. */
  var SETUP_VIEWS = { consent: 'consent', dongle: 'dongle', phone: 'phone' };

  /** Call one PluginHost.setup method. Never throws and never rejects: the
   *  tabs call it from handlers that may have set a busy flag, so a missing
   *  method or a refused call must not leave them stuck. */
  function setupCall(method, args) {
    if (setupMode !== 'on' || !SETUP || typeof SETUP[method] !== 'function') return Promise.resolve(false);
    try {
      return Promise.resolve(SETUP[method].apply(SETUP, args)).then(
        function () {
          return true;
        },
        function () {
          return false;
        }
      );
    } catch (e) {
      return Promise.resolve(false);
    }
  }

  var setupDoneSent = false; // done() told for the current satisfied state
  var setupLastProgress = null;

  /** What the tabs use. In normal mode `active()` is false and every call is
   *  a no-op, so a tab can call these without checking the mode first. */
  var setupApi = {
    active: function () {
      return setupMode === 'on';
    },
    step: function () {
      return setupStep;
    },
    view: function () {
      return setupView;
    },
    /** The step is done (a short detail for the wizard). Told once until
     *  `unsatisfied()` says the state was lost again. */
    done: function (detail) {
      if (setupMode !== 'on' || setupDoneSent) return Promise.resolve(false);
      setupDoneSent = true;
      return setupCall('done', detail ? [String(detail)] : []);
    },
    /** The step's state is not (or no longer) satisfied. */
    unsatisfied: function () {
      setupDoneSent = false;
    },
    /** fraction 0..1 or null, and a short text. Repeats are not sent. */
    progress: function (fraction, text) {
      var key = String(fraction) + '|' + String(text || '');
      if (setupMode !== 'on' || key === setupLastProgress) return Promise.resolve(false);
      setupLastProgress = key;
      return setupCall('progress', [fraction == null ? null : fraction, String(text || '')]);
    },
    /** Only for an error the person must see in the wizard, not one the
     *  tab already shows and can retry. */
    fail: function (message) {
      return setupCall('fail', [String(message || 'This step could not continue.')]);
    },
  };

  window.__aokieTabs = {
    register: function (id, def) {
      tabRegistry[id] = def || {};
    },
    /** Shared helpers for tab modules (defined above in this file). */
    util: {
      esc: esc, errMsg: errMsg, setHtml: setHtml, svg: svg, icons: ICONS, navLink: navLink,
      problemHtml: problemHtml, loadingHtml: LOADING_HTML, plainTimes: plainTimes, sentence: sentence, notRunning: NOT_RUNNING,
      radioDownReason: radioDownReason, lineDownFromPhone: lineDownFromPhone, lineDownFor: lineDownFor,
      focusOf: focusOf, restoreFocus: restoreFocus, focusLost: focusLost,
    },
    /** A tab saved settings: the Overview reads them again when it is next
     *  shown (its summary otherwise waited up to 30 s for its own read). */
    settingsChanged: function () {
      state.settingsDue = true;
    },
    /** Where calls go, shared by the Overview, Settings and Consent tabs. */
    oaiy: OAIY,
    /** Setup mode: whether the screen is a step of OAIY's setup wizard, and
     *  how a tab reports back to it. */
    setup: setupApi,
    switchTo: function (id) {
      switchTab(id);
    },
    /** Shared transport-mode truth (dongle vs built-in Windows Bluetooth) —
     *  tabs read it here and report the settings they load back here. */
    transport: {
      mode: function () {
        return transportMode;
      },
      showDongle: function () {
        return transportMode !== 'native'; // `auto` uses the dongle when one is plugged in
      },
      update: function (settingsBag) {
        setTransportFromSettings(settingsBag);
      },
    },
  };

  // ---- transport mode (dongle vs built-in Windows Bluetooth) --------------
  // `dongle` = the original WinUSB-driver radio; `native` = the built-in
  // Windows Bluetooth transport, where no WinUSB dongle is involved at all;
  // `auto` = the dongle whenever one is plugged in (the plugin starts it so,
  // since only the dongle carries call audio), else native. Native therefore
  // hides every dongle-only surface, and auto shows them as dongle does — the
  // Dongle tab here (and the manifest's setup step); the Settings tab gates
  // its own dongle-only Advanced fields, and the Phone tab adjusts its
  // pairing instructions. The mode
  // ALWAYS comes from the loaded plugin settings; a missing or unknown value
  // means the dongle view (the pre-transportMode behavior — the safe default
  // while the first settings.get is still in flight).

  var transportMode = 'dongle'; // best-known so far; default = the dongle view

  function normalizeTransportMode(value) {
    return value === 'native' || value === 'auto' ? value : 'dongle';
  }

  function setTransportFromSettings(settingsBag) {
    var next = normalizeTransportMode(settingsBag && settingsBag.transportMode);
    if (next === transportMode) return;
    transportMode = next;
    applyTransportVisibility();
  }

  /** Hide/show the dongle-only tab button; if the Dongle tab is somehow the
   *  active one when it becomes hidden, fall back to the Overview. */
  function applyTransportVisibility() {
    // `auto` uses the USB dongle whenever one is plugged in, so its screens
    // show for it as for the dongle mode; only `native` has no dongle.
    var showDongle = transportMode !== 'native';
    var btns = document.querySelectorAll('.rcp-tab-btn');
    for (var i = 0; i < btns.length; i++) {
      if (btns[i].getAttribute('data-tab') === 'dongle') btns[i].hidden = !showDongle;
    }
    // In setup mode the wizard chose the tab; the Dongle tab says itself
    // that the built-in transport needs no dongle.
    if (!showDongle && activeTab === 'dongle' && setupMode === 'off') switchTab('overview');
  }

  function tabContainer(id) {
    return $('tab-' + id);
  }

  /** `fromSetup`: the one switch setup mode makes, to the wizard's view.
   *  Every other switch (tab bar, data-tabgo links, switchTo) is ignored
   *  while setup mode is starting or on: the wizard owns the navigation. */
  function switchTab(id, fromSetup) {
    if (setupMode !== 'off' && !fromSetup) return;
    if (!id || id === activeTab || !tabContainer(id)) return;
    // A tab hidden by the transport mode stays unreachable, even
    // programmatically (data-tabgo links, switchTo).
    if (id === 'dongle' && transportMode === 'native' && !fromSetup) return;

    // Leave the old tab.
    if (activeTab === 'overview') {
      stopTimers();
    } else if (activeDef && typeof activeDef.unmount === 'function') {
      try {
        activeDef.unmount();
      } catch (e) {
        /* a tab's cleanup must never block switching */
      }
    }
    var oldEl = tabContainer(activeTab);
    if (oldEl) oldEl.hidden = true;
    activeDef = null;
    activeTab = id;

    // Tab-bar state.
    var btns = document.querySelectorAll('.rcp-tab-btn');
    for (var i = 0; i < btns.length; i++) {
      var on = btns[i].getAttribute('data-tab') === id;
      btns[i].classList.toggle('is-active', on);
      btns[i].setAttribute('aria-selected', on ? 'true' : 'false');
      btns[i].setAttribute('tabindex', on ? '0' : '-1');
    }

    // Enter the new tab.
    var el = tabContainer(id);
    el.hidden = false;
    if (id === 'overview') {
      // What another tab saved (the greeting, where calls go) shows at once,
      // not at the summary's next 30 s read.
      state.settingsDue = true;
      if (!document.hidden) startTimers();
      return;
    }
    var def = tabRegistry[id];
    if (def && typeof def.mount === 'function') {
      activeDef = def;
      try {
        def.mount(el);
      } catch (e) {
        el.innerHTML =
          '<p class="rcp-error">This tab failed to load: ' + esc(errMsg(e)) + '</p>';
        setupApi.fail('The Aokie screen for this step failed to load: ' + errMsg(e));
      }
    } else {
      el.innerHTML =
        '<p class="rcp-error">This tab did not load: the Aokie screen is incomplete. Reinstall the Aokie plugin, then open it again.</p>';
      setupApi.fail('The Aokie screen for this step did not load. Reinstall the Aokie plugin, then try again.');
    }
  }

  /** Run `fn` once every tab module has registered. The context resolves in
   *  a microtask after THIS script, before the tabs/*.js scripts run. */
  function afterTabScripts(fn) {
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', function () {
        fn();
      });
    } else {
      fn();
    }
  }

  /** PluginHost.setup is there: hide the tab bar and the Overview at once,
   *  then ask which view the wizard wants. */
  function startSetup() {
    var rootEl = $('rcp-root');
    if (rootEl) rootEl.classList.add('is-setup');
    var context;
    try {
      context = Promise.resolve(SETUP.context());
    } catch (e) {
      context = Promise.reject(e);
    }
    context.then(
      function (ctx) {
        if (ctx && ctx.mode === 'setup') {
          afterTabScripts(function () {
            enterSetup(ctx);
          });
        } else {
          leaveSetup();
        }
      },
      function () {
        leaveSetup();
      }
    );
  }

  /** Not a wizard step after all: the normal screen, as without setup. */
  function leaveSetup() {
    setupMode = 'off';
    var rootEl = $('rcp-root');
    if (rootEl) rootEl.classList.remove('is-setup');
    // The host's mark for a wizard step: not one after all, so the tab bar shows.
    var htmlEl = document.documentElement;
    if (htmlEl && htmlEl.removeAttribute) htmlEl.removeAttribute('data-oaiy-setup');
    if (activeTab === 'overview' && !document.hidden) startTimers();
  }

  function enterSetup(ctx) {
    setupMode = 'on';
    setupStep = String(ctx.step || '');
    setupView = String(ctx.view || '');
    var nav = $('rcp-tabs');
    if (nav) nav.hidden = true;
    var overview = tabContainer('overview');
    if (overview) overview.hidden = true;
    var tabId = Object.prototype.hasOwnProperty.call(SETUP_VIEWS, setupView) ? SETUP_VIEWS[setupView] : '';
    if (!tabId || !tabContainer(tabId)) {
      var rootEl = $('rcp-root');
      if (rootEl) {
        rootEl.insertAdjacentHTML(
          'beforeend',
          '<p class="rcp-error">This version of Aokie has no setup view named “' + esc(setupView) + '”.</p>'
        );
      }
      setupApi.fail('This version of Aokie has no setup view named “' + setupView + '”. Update the Aokie plugin, then try again.');
      return;
    }
    switchTab(tabId, true);
  }

  // Manual tab activation: arrows move focus; Enter/Space opens the section.
  // This avoids mounting a settings panel just while navigating past its tab.
  $('rcp-tabs').addEventListener('keydown', function (e) {
    var keys = ['ArrowLeft', 'ArrowRight', 'Home', 'End'];
    if (keys.indexOf(e.key) < 0) return;
    var tabs = Array.prototype.filter.call(document.querySelectorAll('.rcp-tab-btn'), function (button) { return !button.hidden; });
    var index = tabs.indexOf(document.activeElement);
    if (index < 0) return;
    e.preventDefault();
    var next = e.key === 'Home' ? 0 : e.key === 'End' ? tabs.length - 1 : (index + (e.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length;
    tabs.forEach(function (button, i) { button.tabIndex = i === next ? 0 : -1; });
    tabs[next].focus();
    tabs[next].scrollIntoView({ block: 'nearest', inline: 'nearest' });
  });

  // ---- state --------------------------------------------------------------
  // `undefined` = first fetch still in flight (cards show "Loading…");
  // `null` = the latest fetch failed (cards show the current error);
  // otherwise the latest successful payload. Connection and control state
  // must not remain green after the host can no longer verify it.

  var state = {
    snap: undefined, snapError: '',
    phone: undefined, phoneError: '',
    diag: undefined, diagError: '',
    phones: undefined, phonesError: '',
    call: undefined, callKnown: false, callError: '', callPauseReason: '',
    switchboard: undefined, switchboardError: '',
    settings: undefined, settingsError: '',
    sources: undefined, sourcesError: '',
    oaiy: undefined, // PluginHost.oaiyStatus: null = not available
    busyCall: false,
    busyPhones: {}, // address -> true while connect/disconnect runs
    busyRedrive: false,
    // The phone whose Disconnect asks first: a call is on the line (or none
    // can be told), and disconnecting takes it off the receptionist.
    disconnectArm: null,
    // The settings are read on the next tick, not only every sixth: at start,
    // on entering the Overview, and after another tab saved them.
    settingsDue: true,
    now: Date.now(),
  };

  // ---- fetchers -----------------------------------------------------------

  function refreshSnapshot() {
    return HOST.snapshot().then(
      function (snap) {
        state.snap = snap || null;
        state.snapError = snap ? '' : 'The host has no snapshot for the Aokie plugin.';
      },
      function (e) {
        state.snapError = errMsg(e);
        state.snap = null;
      }
    ).then(function () {
      renderHero();
      renderReadiness();
      // Delivery falls back to the snapshot's outbox counts, so it follows the snapshot too.
      if (state.diag !== undefined) renderDelivery();
    });
  }

  function refreshPhoneStatus() {
    return HOST.command('phone.status').then(
      function (data) {
        state.phone = data || null;
        state.phoneError = '';
      },
      function (e) {
        state.phoneError = errMsg(e);
        state.phone = null;
      }
    ).then(function () {
      renderHero(); // a phone that is not connected is not ready for calls
      renderReadiness();
    });
  }

  function refreshDiag() {
    return HOST.command('dongle.diagnostics').then(
      function (data) {
        state.diag = data || null;
        state.diagError = '';
      },
      function (e) {
        // Ordinary hardware outages reject with an
        // explanatory message (it still names the outbox counts) — show it.
        // Consent pauses return a successful structured snapshot. A rejection
        // is a current diagnostics outage, so discard any old paused snapshot
        // instead of masking a post-consent hardware failure.
        state.diagError = errMsg(e);
        state.diag = null;
      }
    ).then(function () {
      renderHero(); // a line paused for consent is not ready for calls
      renderReadiness();
      renderDelivery();
    });
  }

  function refreshPhones() {
    return HOST.command('phone.listPaired').then(
      function (data) {
        var devices = data && data.devices;
        state.phones = (Array.isArray(devices) ? devices : []).map(function (d) {
          if (typeof d === 'string') return { address: d, name: null, connected: false };
          return d || {};
        });
        state.phonesError = '';
      },
      function (e) {
        state.phonesError = errMsg(e);
        state.phones = null;
      }
    ).then(function () {
      renderPhones();
      renderHero();
      renderReadiness(); // the bonded count says "Paired, offline"
    });
  }

  function refreshCall() {
    return HOST.command('call.current').then(
      function (data) {
        var call = data && data.call;
        var radio = data && data.radio;
        state.call = call && call.state !== 'ended' ? call : null;
        state.callPauseReason =
          radio && radio.paused && radio.blockedBy === 'consent'
            ? String(radio.reason || 'Consent must be reviewed before the receptionist can resume.')
            : '';
        state.callKnown = true;
        state.callError = '';
      },
      function (e) {
        state.callError = errMsg(e);
        // Hide stale call controls and an old consent-pause explanation when
        // the authoritative current-call read fails.
        state.call = null;
        state.callKnown = false;
        state.callPauseReason = '';
      }
    ).then(function () {
      renderLive();
      renderPhones(); // Disconnect asks first while a call is on the line
    });
  }

  function refreshSettings() {
    return HOST.command('settings.get').then(
      function (data) {
        state.settings = data || null;
        state.settingsError = '';
        setTransportFromSettings(data && data.settings);
      },
      function (e) {
        state.settingsError = errMsg(e);
        state.settings = null;
      }
    ).then(function () {
      renderSettings();
      renderHero(); // calls that go nowhere are not ready for calls
      renderReadiness(); // the call route names who answers
      renderLive(); // agent-mode gates the operator composer
    });
  }

  /** The host's AI sources — read for OAIY Voice's running state. Older hosts
   *  without aiSources simply leave it unknown. */
  function refreshSources() {
    if (typeof HOST.aiSources !== 'function') return Promise.resolve();
    return HOST.aiSources().then(
      function (list) {
        state.sources = Array.isArray(list) ? list : [];
        state.sourcesError = '';
      },
      function (e) {
        state.sourcesError = errMsg(e);
        state.sources = null;
      }
    ).then(function () {
      renderReadiness();
      renderSettings();
    });
  }

  function refreshOaiyStatus() {
    return readOaiyStatus().then(function (status) {
      state.oaiy = status;
      renderSettings();
    });
  }

  var switchboardRequest = 0;
  function refreshSwitchboard() {
    var request = ++switchboardRequest;
    return HOST.command('call.switchboard').then(function (data) {
      if (!data || typeof data !== 'object' || !('waiting' in data) || !('parked' in data)) {
        throw new Error('The plugin did not return a call waiting snapshot.');
      }
      if (request !== switchboardRequest) return;
      state.switchboard = data;
      state.switchboardError = '';
    }).catch(function (e) {
      if (request !== switchboardRequest) return;
      state.switchboard = null;
      state.switchboardError = errMsg(e);
    }).then(renderSwitchboard);
  }

  // ---- polling (visibility-aware) -----------------------------------------

  var slowTimer = null;
  var fastTimer = null;
  var slowTicks = 0;
  // A tick's reads still out when the next tick comes: that tick is skipped.
  // A slow radio otherwise piled poll after poll up in the plugin's one
  // command queue, ahead of Answer and Hang up.
  var slowBusy = false;
  var fastBusy = false;

  function settled(promises) {
    return Promise.all(
      promises.map(function (p) {
        return Promise.resolve(p).catch(function () {});
      })
    );
  }

  function slowTick() {
    if (slowBusy) return;
    slowBusy = true;
    var reads = [refreshSnapshot(), refreshPhoneStatus(), refreshDiag(), refreshPhones()];
    if (state.settingsDue || slowTicks % 6 === 0) {
      // every ~30 s, at start, on entering the Overview and after a save
      state.settingsDue = false;
      reads.push(refreshSettings());
    }
    if (slowTicks % 3 === 0) {
      // every ~15 s (and at start)
      reads.push(refreshSources());
      reads.push(refreshOaiyStatus());
    }
    slowTicks += 1;
    settled(reads).then(function () {
      slowBusy = false;
    });
  }

  function fastTick() {
    state.now = Date.now();
    if (fastBusy) return;
    fastBusy = true;
    settled([refreshCall(), refreshSwitchboard()]).then(function () {
      fastBusy = false;
    });
  }

  function startTimers() {
    if (slowTimer == null) {
      slowTick();
      slowTimer = window.setInterval(slowTick, 5000);
    }
    if (fastTimer == null) {
      fastTick();
      fastTimer = window.setInterval(fastTick, 2000);
    }
  }

  function stopTimers() {
    if (slowTimer != null) {
      window.clearInterval(slowTimer);
      slowTimer = null;
    }
    if (fastTimer != null) {
      window.clearInterval(fastTimer);
      fastTimer = null;
    }
  }

  // ---- hero ---------------------------------------------------------------

  function pluginHealth() {
    var snap = state.snap;
    if (!snap || typeof snap !== 'object') return null;
    if (snap.lastHealthError) return null;
    // Desktop snapshots carry `lastHealth`; tolerate a plain `health` too.
    return snap.lastHealth || snap.health || null;
  }

  // The outbox counts: dongle.diagnostics' own when it answered, else the
  // plugin's health report, which carries the same counts without needing
  // the radio. A missing dongle is a radio outage, not a delivery one.
  function outboxCounts(diag) {
    if (diag && diag.outbox) return diag.outbox;
    var health = pluginHealth();
    var outbox = health && health.components && health.components.outbox;
    return outbox && typeof outbox === 'object' ? outbox : null;
  }

  function pluginIsLive(snap) {
    return !!(snap && (snap.state === 'running' || snap.state === 'unhealthy'));
  }

  /** The radio's reason in the health report ("radio: no supported USB
   *  Bluetooth dongle was found"), '' when it has none. */
  function healthRadioReason() {
    var health = pluginHealth();
    var m = health && /^radio:\s*(.+)$/i.exec(String(health.detail || ''));
    return m ? m[1] : '';
  }

  /** The phone line is not running: { consent, reason } (consent holds it
   *  back, or the radio did not start), from whichever read says so; null
   *  while it runs or nothing says otherwise. */
  function lineDown() {
    var radio = state.diag && state.diag.radio;
    if (radio && radio.paused && radio.blockedBy === 'consent') return { consent: true, reason: String(radio.reason || '') };
    if (state.callPauseReason) return { consent: true, reason: state.callPauseReason };
    var fromPhone = lineDownFromPhone(state.phone);
    if (fromPhone) return fromPhone;
    var why = state.diag === null ? radioDownReason(state.diagError) : null;
    if (why === null && state.call === null && !state.callKnown) why = radioDownReason(state.callError);
    if (why === null) return null;
    var down = lineDownFor(why);
    if (!down.consent && !down.reason) down.reason = healthRadioReason();
    return down;
  }

  /** "No supported USB Bluetooth dongle was found" and the like: the dongle
   *  itself is missing, not some other fault of the radio. */
  function noDongleReason(reason) {
    return /no (?:supported )?(?:usb )?(?:bluetooth )?dongle/i.test(String(reason || ''));
  }

  function renderHero() {
    var headline = $('hero-headline');
    var sub = $('hero-sub');
    var pill = $('hero-pill');
    var pillText = $('hero-pill-text');
    var version = $('hero-version');
    var snap = state.snap;

    if (snap === undefined) {
      headline.textContent = 'Checking the receptionist…';
      sub.textContent = 'Contacting the Aokie plugin.';
      setPill(pill, pillText, 'is-neutral', 'Checking');
      version.textContent = '';
      return;
    }
    if (snap === null) {
      headline.textContent = 'Plugin status unavailable';
      sub.textContent = state.snapError || 'The host could not describe the Aokie plugin.';
      setPill(pill, pillText, 'is-warn', 'Unknown');
      version.textContent = '';
      return;
    }

    var running = pluginIsLive(snap);
    var health = pluginHealth();
    var status = health ? String(health.status || '') : '';

    // A healthy plugin is not by itself ready for calls: with no phone on the
    // line, or calls sent where nobody answers, the headline said "Ready for
    // calls" over tiles that said otherwise.
    var gaps = status === 'ok' ? readinessGaps() : [];
    // A line held back for consent, or a radio that did not start, is the
    // news: what follows from it (no phone connected) is not.
    var down = status === 'ok' || status === 'degraded' ? lineDown() : null;
    // Records that did not reach OAIY: the delivery card says so, and the
    // headline agrees with it rather than read all well above it.
    var outbox = outboxCounts(state.diag);
    var stuck = outbox ? (Number(outbox.dead) || 0) + (Number(outbox.failed) || 0) : 0;

    if (!running) {
      headline.textContent = 'Aokie is not running';
      sub.textContent = snap.reason || 'Start the plugin from the Plugins workspace to take calls.';
      var stateName = String(snap.state || 'stopped');
      setPill(pill, pillText, snap.state === 'crashed' ? 'is-err' : 'is-neutral', stateName.charAt(0).toUpperCase() + stateName.slice(1));
    } else if (snap.lastHealthError || (snap.state === 'unhealthy' && !health)) {
      headline.textContent = 'Running — needs attention';
      sub.textContent = snap.lastHealthError || snap.reason || 'The plugin is running, but its health could not be verified.';
      setPill(pill, pillText, 'is-warn', 'Needs attention');
    } else if (!health) {
      headline.textContent = 'Starting up';
      sub.textContent = 'No health report from the plugin yet.';
      setPill(pill, pillText, 'is-neutral', 'Starting');
    } else if (down && down.consent) {
      headline.textContent = 'Paused until consent is reviewed';
      sub.textContent =
        (down.reason ? sentence(down.reason).replace(/\.+$/, '') + '. ' : '') +
        'Calls ring through to you until consent is given again, in Consent.';
      setPill(pill, pillText, 'is-warn', 'Consent needed');
    } else if (down) {
      headline.textContent = 'Not ready for calls';
      sub.textContent =
        'Aokie’s phone line is not running' + (down.reason ? ': ' + plainTimes(down.reason).replace(/\.+$/, '') : '') + '.' +
        (transportMode === 'native' ? '' : ' Set up the dongle in the Dongle tab.');
      setPill(pill, pillText, 'is-warn', 'Not ready');
    } else if (status === 'ok' && gaps.length) {
      headline.textContent = 'Not ready for calls';
      sub.textContent = gaps.join(' ');
      setPill(pill, pillText, 'is-warn', 'Not ready');
    } else if (status === 'ok' && stuck > 0) {
      var dead = Number(outbox.dead) || 0;
      headline.textContent = 'Ready for calls · some records waiting';
      sub.textContent = dead > 0
        ? (dead === 1 ? 'One call or text record' : dead + ' call and text records') +
          ' did not reach OAIY: send ' + (dead === 1 ? 'it' : 'them') + ' again under Call & text records.'
        : 'Some call and text records have not reached OAIY yet: Aokie keeps trying.';
      setPill(pill, pillText, 'is-warn', 'Records waiting');
    } else if (status === 'ok') {
      headline.textContent = 'Ready for calls';
      sub.textContent = sentence(health.detail) || 'Aokie is running and its phone line is up.';
      setPill(pill, pillText, 'is-ok', 'Healthy');
    } else if (status === 'degraded') {
      headline.textContent = 'Running — needs attention';
      sub.textContent = sentence(health.detail) || 'The plugin reports a degraded component.';
      setPill(pill, pillText, 'is-warn', 'Degraded');
    } else {
      headline.textContent = 'Plugin health: ' + (status || 'unknown');
      sub.textContent = sentence(health.detail) || 'See the readiness checks below.';
      setPill(pill, pillText, status === 'error' ? 'is-err' : 'is-warn', status || 'Unknown');
    }

    var pluginVersion = snap.version || (snap.manifest && snap.manifest.version);
    version.textContent = pluginVersion ? 'Aokie v' + pluginVersion : '';
  }

  /** What keeps a healthy plugin from taking calls, as far as it is known
   *  (a read still out, or failed, is not counted against it). */
  function readinessGaps() {
    var gaps = [];
    var phone = state.phone;
    var radio = state.diag && state.diag.radio;
    var paused = !!(radio && radio.paused && radio.blockedBy === 'consent');
    // (first, and alone of the phone's gaps: no phone connects while it lasts)
    if (paused) gaps.push('The phone line is paused until consent is given again.');
    if (!paused && phone && typeof phone === 'object') {
      if (phone.pairingConfirm) gaps.push('A phone is waiting for its pairing code to be confirmed in Phone setup.');
      else if (!phone.connected) {
        gaps.push(
          (Array.isArray(state.phones) && state.phones.length) || phone.paired
            ? 'The paired phone is not connected.'
            : 'No phone is paired yet: pair one in Phone setup.'
        );
      }
    }
    var route = state.settings ? OAIY.callRoute(state.settings.settings) : 'unknown';
    if (route === 'local' || route === 'flows') {
      gaps.push('Calls do not go to OAIY, so nobody answers them: send them to OAIY in Settings.');
    }
    return gaps;
  }

  // ---- readiness grid -----------------------------------------------------

  function readinessItems() {
    var items = [];
    var snap = state.snap;
    var running = pluginIsLive(snap);
    var health = pluginHealth();
    var phone = state.phone;
    var diag = state.diag;

    // Phone bridge (phone.status)
    (function () {
      var label = 'Phone';
      if (phone === undefined) {
        items.push({ icon: ICONS.smartphone, label: label, value: '…', note: 'Checking…', ok: null });
        return;
      }
      if (phone === null) {
        var off = state.phoneError === NOT_RUNNING;
        items.push({ icon: ICONS.smartphone, label: label, value: off ? '—' : 'Unavailable', note: off ? 'Aokie is not running' : state.phoneError || 'The phone status could not be read', ok: off ? null : false });
        return;
      }
      // Linked is what phone.status says of the link: an old error it still
      // carries is shown, but does not unlink a connected phone. A phone
      // bonded but not connected now is paired and offline (`paired` is about
      // the connection; the bonded list says who is paired).
      var linked = !!phone.connected && !phone.pairingConfirm;
      var bondedCount = Array.isArray(state.phones) ? state.phones.length : 0;
      // No phone line (consent holds it back, or the radio did not start):
      // the phone waits on that, and the tile says where it is put right
      // rather than offer a pairing that cannot happen. The radio tile says why.
      var down = lineDownFromPhone(phone);
      if (down) {
        items.push({
          icon: ICONS.smartphone,
          label: label,
          value: bondedCount > 0 || phone.paired ? 'Paired, offline' : 'Not paired',
          note: down.consent
            ? 'Reconnects once consent is given again'
            : bondedCount > 0 || phone.paired
              ? 'Reconnects once the phone line is running'
              : 'A phone can pair once the phone line is running',
          ok: null,
          go: down.consent ? ['consent', 'Review consent'] : transportMode === 'native' ? null : ['dongle', 'Dongle setup'],
        });
        return;
      }
      var value = phone.pairingConfirm
        ? 'Confirm pairing'
        : linked
          ? 'Bluetooth linked'
          : phone.error
            ? 'Needs attention'
            : bondedCount > 0 || phone.paired
              ? 'Paired, offline'
              : 'Not paired';
      var device = phone.device || null;
      var named = device && (device.name || device.address);
      var note = linked
        ? (named || 'Phone connected') + (phone.error ? ' · last error: ' + phone.error : '')
        : phone.error || named || (bondedCount > 0 ? 'The paired phone is not connected' : 'Pair a phone with “Aokie AI Assistant”');
      items.push({
        icon: ICONS.smartphone,
        label: label,
        value: value,
        note: note,
        ok: linked,
        go: linked ? null : ['phone', phone.pairingConfirm ? 'Confirm the code' : bondedCount > 0 || phone.paired ? 'Phone setup' : 'Pair a phone'],
      });
    })();

    // Who answers, from the plugin's health report. Its realtime error is the
    // last failed call's and stays until a call connects again; `ready` is
    // only true inside a call, so neither is a probe of the gateway.
    (function () {
      var label = 'Calls answered by';
      var responder = health && health.components && health.components.responder;
      if (!running) {
        items.push({
          icon: ICONS.server,
          label: label,
          value: snap === undefined ? '…' : '—',
          note: snap === undefined ? 'Checking…' : snap === null ? 'The plugin status could not be read' : 'Aokie is not running',
          ok: null,
        });
        return;
      }
      if (snap.lastHealthError || (snap.state === 'unhealthy' && !health)) {
        items.push({ icon: ICONS.server, label: label, value: 'Unavailable', note: snap.lastHealthError || snap.reason || 'The health check did not answer', ok: false });
        return;
      }
      if (!responder) {
        items.push({ icon: ICONS.server, label: label, value: '…', note: health ? 'Not in the latest health report' : 'No health report yet', ok: null });
        return;
      }
      if (responder.mode === 'desktop_realtime') {
        var radioHealth = health.components.radio;
        var realtime = radioHealth && radioHealth.voiceRuntime && radioHealth.voiceRuntime.realtime;
        var lastError = responder.realtimeError || (realtime && realtime.error) || '';
        var failure = lastError ? 'last call: ' + lastError : '';
        var bag = state.settings && state.settings.settings;
        var toOaiy = OAIY.realtimeProviderId(bag && bag.realtimeVoiceEndpoint) === OAIY.providerId;
        if (!toOaiy) {
          items.push({
            icon: ICONS.server,
            label: label,
            value: bag ? 'Another realtime provider' : 'Realtime provider',
            note: failure || (bag ? 'Not OAIY — send calls to OAIY in Settings' : 'Checking where calls go'),
            ok: failure ? false : null,
            go: bag ? ['settings', 'Settings'] : null,
          });
          return;
        }
        // OAIY Voice hears and speaks for every call; without it the Front
        // desk cannot answer. Unknown (no source list) is not a failure.
        var voice = state.sources ? OAIY.voiceService(state.sources) : null;
        var voiceDown = !!voice && voice.status !== 'running';
        items.push({
          icon: ICONS.server,
          label: label,
          value: 'OAIY Front desk',
          note:
            failure ||
            (voiceDown ? 'OAIY Voice is ' + voice.status + ' — start it in OAIY Services' : '') ||
            (voice ? 'OAIY Voice running · the Front desk agent replies' : 'The Front desk agent replies through OAIY'),
          ok: failure || voiceDown ? false : voice ? true : null,
        });
        return;
      }
      // Off the realtime route the receptionist would be Aokie's own, and
      // flow or operator replies would be said in its voice: Aokie has no
      // speech of its own, so nobody answers and calls ring through.
      items.push({
        icon: ICONS.server,
        label: label,
        value: 'Nobody',
        note: 'Calls ring through to you until you send them to OAIY in Settings.',
        ok: false,
        go: ['settings', 'Send calls to OAIY'],
      });
    })();

    // Radio & dongle (dongle.diagnostics)
    (function () {
      var label = 'Radio & dongle';
      if (diag === undefined) {
        items.push({ icon: ICONS.radio, label: label, value: '…', note: 'Checking…', ok: null });
        return;
      }
      var radio = diag && diag.radio;
      // The radio did not start (no dongle, one another program holds): its
      // reason in plain words, once, and the Dongle tab to put it right.
      var notStarted = !radio && state.diagError !== NOT_RUNNING ? radioDownReason(state.diagError) : null;
      if (notStarted !== null) {
        var line = lineDown();
        if (line && line.consent) {
          items.push({ icon: ICONS.radio, label: label, value: 'Paused for consent', note: sentence(line.reason) || 'Consent must be reviewed', ok: false, go: ['consent', 'Review consent'] });
          return;
        }
        var why = (line && line.reason) || notStarted || healthRadioReason();
        items.push({
          icon: ICONS.radio,
          label: label,
          value: noDongleReason(why) ? 'No dongle' : 'Not running',
          note: why ? sentence(why) : 'Aokie’s radio is not running',
          ok: false,
          go: transportMode === 'native' ? null : ['dongle', 'Dongle setup'],
        });
        return;
      }
      if (!radio) {
        var off = state.diagError === NOT_RUNNING;
        items.push({
          icon: ICONS.radio,
          label: label,
          value: off ? '—' : 'Unavailable',
          note: off ? 'Aokie is not running' : state.diagError || 'The radio did not report',
          ok: off ? null : false,
          go: off || transportMode === 'native' ? null : ['dongle', 'Dongle setup'],
        });
        return;
      }
      var consentPaused = !!radio.paused && radio.blockedBy === 'consent';
      // (Its voice slots always say Aokie has no speech of its own: that is
      // not a radio fault, and "Calls answered by" says what it means.)
      var value = consentPaused ? 'Paused for consent' : radio.initialized ? 'Ready' : 'Not started';
      var note =
        (consentPaused ? sentence(radio.reason) : '') ||
        radio.error ||
        (radio.initialized ? 'The radio is up' + (radio.localAddress ? ' (' + radio.localAddress + ')' : '') : 'Plug in a supported dongle');
      items.push({
        icon: ICONS.radio,
        label: label,
        value: value,
        note: note,
        ok: !consentPaused && !!radio.initialized && !radio.error,
        // (no Dongle tab on the built-in transport: nothing to link to there)
        go: consentPaused ? ['consent', 'Review consent'] : radio.initialized && !radio.error || transportMode === 'native' ? null : ['dongle', 'Dongle setup'],
      });
    })();

    // Call and text records on their way to OAIY (outbox counts)
    (function () {
      var label = 'Call & text records';
      if (diag === undefined) {
        items.push({ icon: ICONS.cloud, label: label, value: '…', note: 'Checking…', ok: null });
        return;
      }
      var outbox = outboxCounts(diag);
      if (!outbox) {
        var off = state.diagError === NOT_RUNNING;
        items.push({ icon: ICONS.cloud, label: label, value: off ? '—' : 'Unknown', note: off ? 'Aokie is not running' : 'Aokie did not report its delivery counts', ok: null });
        return;
      }
      var pending = Number(outbox.pending) || 0;
      var failed = Number(outbox.failed) || 0;
      var dead = Number(outbox.dead) || 0;
      var healthy = dead === 0 && failed === 0;
      items.push({
        icon: ICONS.cloud,
        label: label,
        value: healthy ? (pending > 0 ? 'Delivering…' : 'All delivered') : 'Needs attention',
        note: pending + ' waiting · ' + failed + ' retrying · ' + dead + ' gave up',
        ok: healthy,
      });
    })();

    return items;
  }

  function renderReadiness() {
    var html = readinessItems()
      .map(function (it) {
        var stateCls = it.ok == null ? 'is-unknown' : it.ok ? 'is-ok' : 'is-bad';
        var stateIcon = it.ok == null ? '' : it.ok ? ICONS.check : ICONS.alert;
        var note = plainTimes(it.note);
        return (
          // The dashboard's status tile: icon, the value (coloured by its
          // state), what it is, then the detail, clamped with the full text
          // in its title, and where to put it right when it needs it.
          '<div class="rcp-ready ' + stateCls + '">' +
          '<span class="rcp-ready__icon">' + it.icon + '</span>' +
          '<strong>' + esc(it.value) + '</strong>' +
          '<small>' + esc(it.label) + '</small>' +
          '<p title="' + esc(note) + '">' + esc(note) + '</p>' +
          (it.go ? '<button type="button" class="rcp-link-btn" data-tabgo="' + it.go[0] + '">' + esc(it.go[1]) + ' ›</button>' : '') +
          '<span class="rcp-ready__state ' + stateCls + '">' + stateIcon + '</span>' +
          '</div>'
        );
      })
      .join('');
    setHtml($('readiness'), html);
  }

  // ---- live call ----------------------------------------------------------

  /** Who owns replies right now: 'agent' | 'operator' | 'unknown' (settings
   *  not loaded yet — treat as agent-owned so the composer never lies). */
  function replyOwner() {
    var bag = state.settings && state.settings.settings;
    if (!bag) return 'unknown';
    // Older settings stores may contain string booleans. Missing or invalid
    // values must not enable operator speech while the AI owns the call.
    if (bag.aiReceptionist === true || bag.aiReceptionist === 'true') return 'agent';
    if (bag.aiReceptionist === false || bag.aiReceptionist === 'false') return 'operator';
    return 'unknown';
  }

  function callDuration(call) {
    if (!call || !call.startedAt) return null;
    var t = Date.parse(call.startedAt);
    if (isNaN(t)) return null;
    var secs = Math.max(0, Math.floor((state.now - t) / 1000));
    var m = Math.floor(secs / 60);
    var s = secs % 60;
    return (m < 10 ? '0' : '') + m + ':' + (s < 10 ? '0' : '') + s;
  }

  function renderLive() {
    var title = $('live-title');
    var pill = $('live-pill');
    var pillText = $('live-pill-text');
    var body = $('live-body');
    var speakRow = $('speak-row');
    var input = $('speak-input');
    var send = $('speak-send');
    var call = state.call;

    if (call === undefined) {
      title.textContent = 'Checking for a live call…';
      pill.hidden = true;
      speakRow.hidden = true;
      setHtml(body, LOADING_HTML);
      return;
    }

    if (!call) {
      pill.hidden = true;
      speakRow.hidden = true;
      // The radio not running is a state of the line, not a failed read: the
      // card says so once, calmly, and where to put it right (the radio tile
      // above gives the reason).
      var downWhy = state.callKnown ? null : radioDownReason(state.callError);
      var pauseWhy = state.callPauseReason;
      if (downWhy !== null && lineDownFor(downWhy).consent) {
        pauseWhy = lineDownFor(downWhy).reason || 'Consent must be reviewed before the receptionist can resume.';
        downWhy = null;
      }
      if (downWhy !== null) {
        title.textContent = 'No phone line';
        setHtml(
          body,
          '<div class="rcp-empty-call">' +
            '<span class="rcp-empty-call__icon">' + ICONS.phoneBig + '</span>' +
            '<h4>The phone line is not running</h4>' +
            '<p>Calls show here once Aokie’s radio is running' + (transportMode === 'native' ? '.' : ' on the dongle.') + '</p>' +
            (transportMode === 'native'
              ? ''
              : '<div class="rcp-actions"><button type="button" class="rcp-button" data-tabgo="dongle">Open the Dongle tab</button></div>') +
            '</div>'
        );
        return;
      }
      if (!state.callKnown && !pauseWhy) {
        title.textContent = 'Live call unavailable';
        setHtml(body, problemHtml(state.callError || 'The current call could not be read.'));
        return;
      }
      if (pauseWhy) {
        title.textContent = 'Receptionist paused';
        setHtml(
          body,
          '<div class="rcp-empty-call">' +
            '<span class="rcp-empty-call__icon is-warn">' + ICONS.alert + '</span>' +
            '<h4>Consent needs review</h4>' +
            '<p>' + esc(sentence(pauseWhy).replace(/\.+$/, '')) + '. Calls ring through to you until consent is given again.</p>' +
            '<div class="rcp-actions"><button type="button" class="rcp-button is-primary" data-tabgo="consent">Review consent</button></div>' +
            '</div>'
        );
        return;
      }
      title.textContent = 'No active call';
      setHtml(
        body,
        '<div class="rcp-empty-call">' +
          '<span class="rcp-empty-call__icon">' + ICONS.phoneBig + '</span>' +
          '<h4>Waiting for the next call</h4>' +
          '<p>When a call rings on the paired phone it shows here, with Answer and Reject.</p>' +
          '</div>'
      );
      return;
    }

    var caller = call.from || 'Unknown caller';
    var outbound = call.direction === 'outbound';

    if (call.state === 'ringing') {
      title.textContent = outbound ? 'Calling out' : 'Incoming call';
      setPill(pill, pillText, 'is-warn', outbound ? 'Dialing' : 'Ringing');
      speakRow.hidden = true;
      setHtml(
        body,
        '<div class="rcp-ringing">' +
          '<h4>' + esc(caller) + '</h4>' +
          (outbound
            ? '<p>Waiting for the other person to answer.</p>'
            : call.leftToOwner === 'ringing'
              ? '<p>Not on your allowed numbers, so it is left ringing for you. Answer hands the call to the receptionist.</p>'
              : '') +
          '<div class="rcp-call-actions">' +
          (outbound ?
          '<button type="button" class="rcp-call-action is-reject" data-act="call-hangup"' + (state.busyCall ? ' disabled' : '') + '>' +
          ICONS.phone + ' Cancel call</button>' :
          '<button type="button" class="rcp-call-action is-reject" data-act="call-reject"' + (state.busyCall ? ' disabled' : '') + '>' +
          ICONS.x + ' Reject</button>' +
          '<button type="button" class="rcp-call-action is-answer" data-act="call-answer"' + (state.busyCall ? ' disabled' : '') + '>' +
          ICONS.phone + ' Answer</button>') +
          '</div></div>'
      );
      return;
    }

    // Active call.
    title.textContent = 'On a call';
    setPill(pill, pillText, 'is-live', callDuration(call) || 'Live call');
    // A call left ringing for the owner that they answered on the phone is
    // theirs: the receptionist neither speaks nor listens.
    var taken = call.leftToOwner === 'taken';
    var owner = taken ? 'owner' : replyOwner();
    // What is typed here would be said in Aokie's own voice, which it does
    // not have: OAIY Voice speaks on calls that go to OAIY.
    var voiceless = owner === 'operator';
    var note = taken
      ? 'You answered this call on the phone, so the receptionist stays out of it.'
      : owner === 'agent'
        ? 'The AI receptionist is handling this call.'
        : voiceless
          ? NO_OWN_SPEECH + ', so nothing can be said to the caller from here.'
          : 'Checking who is handling this call…';
    setHtml(
      body,
      '<div class="rcp-ringing is-active">' +
        '<small>' + (outbound ? 'OUTBOUND CALL' : 'INCOMING CALL') + '</small>' +
        '<h4>' + esc(caller) + '</h4>' +
        '<p>' + esc(note) + '</p>' +
        '<div class="rcp-call-actions">' +
        '<button type="button" class="rcp-call-action is-reject" data-act="call-hangup"' + (state.busyCall ? ' disabled' : '') + '>' +
        ICONS.phone + ' Hang up</button>' +
        '</div></div>'
    );
    var muted = owner !== 'operator' || voiceless;
    // A composer that can never speak is not shown: with no voice of its own
    // Aokie cannot say what is typed, and the note above says why.
    speakRow.hidden = taken || muted;
    input.disabled = state.busyCall || muted;
    input.placeholder = voiceless
      ? 'Aokie has no voice of its own'
      : muted
        ? 'Disabled while the AI receptionist replies'
        : 'Say something to the caller…';
    send.disabled = state.busyCall || muted;
  }

  // ---- delivery -----------------------------------------------------------

  function renderSwitchboard() {
    var body = $('switchboard-body');
    var board = state.switchboard;
    // (Aokie or its radio not running: the live call card says so already,
    // once is enough)
    body.hidden =
      board === undefined ||
      !!(board && !board.waiting && !board.parked && !board.switchInProgress) ||
      (!board && (state.switchboardError === NOT_RUNNING || radioDownReason(state.switchboardError) !== null));
    if (body.hidden) {
      setHtml(body, '');
      return;
    }
    if (!board) {
      setHtml(body, '<p class="rcp-error">Call waiting status unavailable: ' + esc(state.switchboardError) + '</p>');
      return;
    }
    var html = board.switchInProgress ? '<p class="rcp-switchboard__transition">Switching callers… Waiting for the phone to confirm.</p>' : '';
    [['waiting', 'Waiting caller'], ['parked', 'On hold']].forEach(function (entry) {
      var leg = board[entry[0]];
      if (!leg) return;
      html += '<div class="rcp-switchboard__leg"><strong>' + entry[1] + '</strong><span>' + esc(leg.from || 'Unknown number') + '</span></div>';
    });
    setHtml(body, html);
  }

  function renderDelivery() {
    var title = $('delivery-title');
    var pill = $('delivery-pill');
    var pillText = $('delivery-pill-text');
    var body = $('delivery-body');
    var diag = state.diag;

    if (diag === undefined) {
      title.textContent = 'Checking delivery…';
      setPill(pill, pillText, 'is-neutral', 'Checking');
      setHtml(body, LOADING_HTML);
      return;
    }

    var outbox = outboxCounts(diag);
    if (!outbox) {
      title.textContent = 'Delivery counts unavailable';
      setPill(pill, pillText, 'is-neutral', 'Unknown');
      setHtml(
        body,
        state.diagError === NOT_RUNNING || !state.diagError
          ? problemHtml(state.diagError || 'Aokie did not report its delivery counts.')
          : '<p class="rcp-error">Aokie could not report its delivery counts: ' + esc(state.diagError) + '</p>'
      );
      return;
    }

    var pending = Number(outbox.pending) || 0;
    var failed = Number(outbox.failed) || 0;
    var dead = Number(outbox.dead) || 0;
    var collisions = Number(outbox.keyCollisions) || 0;
    var healthy = dead === 0 && failed === 0;

    title.textContent = healthy
      ? pending > 0
        ? 'Delivering to OAIY…'
        : 'All delivered to OAIY'
      : dead > 0
        ? 'Some records were not delivered'
        : 'Retrying delivery';
    if (healthy) setPill(pill, pillText, 'is-ok', 'Healthy');
    else if (dead > 0) setPill(pill, pillText, 'is-warn', dead + ' gave up');
    else setPill(pill, pillText, 'is-warn', failed + ' retrying');

    var html =
      '<div class="rcp-metrics">' +
      '<span><strong>' + pending + '</strong><small>Waiting</small></span>' +
      '<span' + (failed ? ' class="is-warn"' : '') + '><strong>' + failed + '</strong><small>Retrying</small></span>' +
      '<span' + (dead ? ' class="is-warn"' : '') + '><strong>' + dead + '</strong><small>Gave up</small></span>' +
      '</div>';
    if (dead > 0) {
      html +=
        '<p class="rcp-delivery-note">OAIY did not take ' + (dead === 1 ? 'one record' : dead + ' records') +
        ' after days of trying. Send ' + (dead === 1 ? 'it' : 'them') + ' again once OAIY is running.</p>';
    }
    if (collisions > 0) {
      html +=
        '<p class="rcp-delivery-note is-warn">' +
        (collisions === 1 ? 'One record was' : collisions + ' records were') +
        ' turned away as a copy of another. That is a fault in Aokie: its log has the details.</p>';
    }
    if (dead > 0) {
      html +=
        '<div class="rcp-actions">' +
        '<button type="button" class="rcp-button" data-act="redrive"' + (state.busyRedrive ? ' disabled' : '') + '>' +
        (state.busyRedrive
          ? 'Sending again…'
          : 'Send ' + (dead === 1 ? 'it' : 'the ' + dead) + ' again') +
        '</button></div>';
    }
    setHtml(body, html);
  }

  // ---- paired phones ------------------------------------------------------

  /** What Disconnect would do to a call: the words of its confirm, or ''
   *  when no call is on the line (then it asks nothing). */
  function disconnectWarning() {
    var call = state.call;
    if (call) {
      var who = call.from || 'the caller';
      return call.state === 'ringing' && call.direction !== 'outbound'
        ? 'A call from ' + who + ' is ringing: once the phone is disconnected, the receptionist cannot answer it.'
        : 'This ends the call with ' + who + '.';
    }
    // (the current call could not be read: it may be there)
    return state.callKnown ? '' : 'Aokie cannot tell whether a call is on the line: if one is, this ends it.';
  }

  function renderPhones() {
    var title = $('phones-title');
    var body = $('phones-body');
    var phones = state.phones;

    if (phones === undefined) {
      title.textContent = 'Checking paired phones…';
      setHtml(body, LOADING_HTML);
      return;
    }
    if (phones === null) {
      // (a list the radio could not give for not running: no failure of its own)
      var listDown = radioDownReason(state.phonesError) !== null;
      title.textContent = listDown ? 'Not known yet' : 'Paired phones unavailable';
      setHtml(
        body,
        listDown
          ? '<p class="rcp-empty">The paired phones show here once the phone line is running.</p>'
          : problemHtml(state.phonesError || 'The paired phones could not be read.')
      );
      return;
    }
    // No phone line: nothing here can pair, reconnect or disconnect a phone
    // until it runs, so the card says what it waits on instead.
    var down = lineDown();
    var goFix = down
      ? down.consent
        ? '<button type="button" class="rcp-button" data-tabgo="consent">Review consent</button>'
        : transportMode === 'native'
          ? ''
          : '<button type="button" class="rcp-button" data-tabgo="dongle">Set up the dongle</button>'
      : '';
    if (phones.length === 0) {
      title.textContent = 'No phones paired';
      setHtml(
        body,
        down
          ? '<p class="rcp-empty">No phone is paired with Aokie yet. A phone can pair once ' +
              (down.consent ? 'consent is given again.' : 'the phone line is running.') + '</p>' +
              (goFix ? '<div class="rcp-actions">' + goFix + '</div>' : '')
          : '<p class="rcp-empty">No phone is paired with Aokie yet.</p>' +
              '<div class="rcp-actions"><button type="button" class="rcp-button" data-tabgo="phone">Pair a phone</button></div>'
      );
      return;
    }

    // Disconnect asks first while a call is on the line; with none now, the
    // question is dropped.
    var warning = disconnectWarning();
    if (!warning) state.disconnectArm = null;

    title.textContent = phones.length + ' phone' + (phones.length === 1 ? '' : 's') + ' paired';
    var html =
      '<div class="rcp-phone-list">' +
      phones
        .map(function (d) {
          var addr = String(d.address || '');
          var busy = !!state.busyPhones[addr];
          var connected = !!d.connected;
          var armed = state.disconnectArm === addr && connected && !down;
          return (
            '<div class="rcp-phone-row">' +
            '<span class="rcp-phone-row__icon' + (connected ? ' is-ok' : '') + '">' + ICONS.smartphone + '</span>' +
            '<span class="rcp-phone-row__id">' +
            '<strong>' + esc(d.name || 'Unnamed phone') + '</strong>' +
            '<small>' + esc(addr) + '</small>' +
            '</span>' +
            '<span class="rcp-phone-row__actions">' +
            '<span class="rcp-pill ' + (connected ? 'is-ok' : 'is-neutral') + '"><i></i>' +
            (connected ? 'Connected' : 'Offline') +
            '</span>' +
            (down
              ? ''
              : '<button type="button" class="rcp-button is-small" data-act="' +
                (connected ? 'phone-disconnect' : 'phone-connect') +
                '" data-addr="' + esc(addr) + '"' + (busy || armed ? ' disabled' : '') + '>' +
                (busy
                  ? connected
                    ? 'Disconnecting…'
                    : 'Reconnecting…'
                  : connected
                    ? 'Disconnect'
                    : 'Reconnect') +
                '</button>') +
            '</span>' +
            (armed
              ? '<div class="rcp-confirm is-danger" role="group" aria-label="Disconnect while a call is on the line">' +
                '<p><strong>Disconnect ' + esc(d.name || 'this phone') + '?</strong> ' + esc(warning) + '</p>' +
                '<div class="rcp-actions">' +
                '<button type="button" class="rcp-button is-danger" data-act="phone-disconnect-confirm" data-addr="' + esc(addr) + '"' + (busy ? ' disabled' : '') + '>Disconnect anyway</button>' +
                '<button type="button" class="rcp-button" data-act="phone-disconnect-cancel" data-addr="' + esc(addr) + '">Keep it connected</button>' +
                '</div>' +
                '</div>'
              : '') +
            '</div>'
          );
        })
        .join('') +
      '</div>' +
      (down
        ? '<p class="rcp-empty">' +
          (down.consent ? 'Phones reconnect by themselves once consent is given again.' : 'Phones reconnect by themselves once the phone line is running.') +
          '</p>'
        : '');
    setHtml(body, html);
  }

  // ---- settings summary (read-only) ---------------------------------------

  var ROUTE_LABEL = {
    oaiy: 'OAIY Front desk',
    realtime: 'Another realtime provider',
    local: 'Nobody: calls ring through',
    flows: 'Nobody: calls ring through',
    unknown: '—',
  };

  /** OAIY Voice's state from the host's source list, as a summary value. */
  function oaiyVoiceLabel() {
    if (state.sources === undefined) return 'Checking…';
    if (state.sources === null) return 'Unknown';
    var voice = OAIY.voiceService(state.sources);
    if (!voice) return 'Not installed';
    return voice.status === 'running' ? 'Running' : voice.status.charAt(0).toUpperCase() + voice.status.slice(1);
  }

  function renderSettings() {
    var title = $('settings-title');
    var body = $('settings-body');
    var s = state.settings;

    if (s === undefined) {
      title.textContent = 'Checking the settings…';
      setHtml(body, LOADING_HTML);
      return;
    }
    if (s === null) {
      title.textContent = 'Settings unavailable';
      setHtml(body, problemHtml(state.settingsError || 'The settings could not be read.'));
      return;
    }

    var bag = s.settings || {};
    var route = OAIY.callRoute(bag);
    var greetingSet = typeof bag.greeting === 'string' && bag.greeting.trim() !== '';
    var briefSet = typeof bag.persona === 'string' && bag.persona.trim() !== '';
    var autoAnswer = bag.autoAnswer === true || bag.autoAnswer === 'true';

    // (No AI receptionist is not "flow-driven replies": flows spoke in
    // Aokie's own voice, which is gone, so those calls go nowhere too.)
    title.textContent =
      route === 'oaiy' ? 'Calls go to OAIY' :
      route === 'unknown' ? 'Receptionist settings' : 'Calls do not go to OAIY';

    // [label, value, page?] — with a page, the value opens the dashboard page
    // where it is set.
    var rows = [['Calls answered by', ROUTE_LABEL[route], route === 'oaiy' ? 'agent' : null]];
    if (route === 'oaiy') {
      var voiceName = OAIY.voiceText(state.oaiy);
      var model = OAIY.modelText(state.oaiy);
      rows.push(['OAIY Voice', oaiyVoiceLabel(), 'services']);
      // (the voice card, How the phone sounds, is on Calendar › Hours & Services)
      rows.push(['Voice on calls', voiceName || 'Chosen in Hours & Services', 'hours']);
      rows.push(['Model', model || 'Loaded in Engines', 'engines']);
    }
    rows.push(
      ['Greeting', greetingSet ? 'Custom greeting set' : 'Default greeting'],
      ['Receptionist brief', briefSet ? 'Custom brief set' : 'Built-in brief'],
      ['Auto-answer', autoAnswer ? 'On' : 'Off']
    );
    var html =
      '<div class="rcp-settings-list">' +
      rows
        .map(function (r) {
          return (
            '<div class="rcp-setting"><small>' + esc(r[0]) + '</small>' +
            '<span title="' + esc(r[1]) + '">' + (r[2] ? navLink(r[2], r[1]) : esc(r[1])) + '</span></div>'
          );
        })
        .join('') +
      '</div>';
    setHtml(body, html);
  }

  // ---- actions ------------------------------------------------------------

  /** Run one call control with the KNOWN callId; `stale_call` refusals refresh
   *  instead of retrying blindly (the call changed under the operator).
   *  Resolves true only when the command succeeded. */
  function callAct(command, extra) {
    if (!state.call || state.busyCall) return Promise.resolve(false);
    state.busyCall = true;
    renderLive();
    var body = { callId: state.call.callId };
    if (extra) {
      for (var k in extra) {
        if (Object.prototype.hasOwnProperty.call(extra, k)) body[k] = extra[k];
      }
    }
    return HOST.command(command, body)
      .then(
        function () {
          return refreshCall().then(function () {
            return true;
          });
        },
        function (e) {
          var msg = errMsg(e);
          // OAIY's bridge hands the plugin's typed error on as `code`; an
          // older host has only the words (errMsg capitalises their start).
          if ((e && e.code === 'stale_call') || /stale_call/i.test((e && e.message) || String(e))) {
            HOST.toast('info', 'The call changed meanwhile: showing it as it is now.');
            return refreshCall().then(function () {
              return false;
            });
          }
          HOST.toast('error', 'The call could not be changed: ' + msg);
          return false;
        }
      )
      .then(function (ok) {
        state.busyCall = false;
        renderLive();
        return ok;
      });
  }

  function sendSpeech() {
    var input = $('speak-input');
    var text = input.value.trim();
    if (!text || !state.call || state.call.state !== 'active') return;
    callAct('call.operatorSpeak', { text: text }).then(function (ok) {
      if (ok) input.value = '';
    });
  }

  function phoneAct(command, address) {
    if (!address || state.busyPhones[address]) return;
    state.busyPhones[address] = true;
    renderPhones();
    HOST.command(command, { address: address })
      .then(
        function () {
          // Both commands are accepted-style: the radio finishes the work
          // asynchronously and the roster poll/events report the real outcome.
          HOST.toast(
            'info',
            command === 'phone.connect'
              ? 'Reconnecting the phone (' + address + '): it shows as connected once it is back.'
              : 'Disconnecting the phone (' + address + '): it may take a moment.'
          );
          return refreshPhones().then(refreshPhoneStatus);
        },
        function (e) {
          HOST.toast(
            'error',
            (command === 'phone.connect' ? 'The phone could not be reconnected: ' : 'The phone could not be disconnected: ') + errMsg(e)
          );
        }
      )
      .then(function () {
        delete state.busyPhones[address];
        renderPhones();
      });
  }

  function redriveDead() {
    if (state.busyRedrive) return;
    state.busyRedrive = true;
    renderDelivery();
    HOST.command('outbox.redrive', { all: true })
      .then(
        function (data) {
          var revived = (data && data.revived) || 0;
          HOST.toast(
            revived > 0 ? 'success' : 'info',
            revived > 0
              ? 'Sending ' + (revived === 1 ? 'the record' : revived + ' records') + ' to OAIY again.'
              : 'Nothing to send again: every record has been delivered.'
          );
          return refreshDiag();
        },
        function (e) {
          HOST.toast('error', 'The records could not be sent again: ' + errMsg(e));
        }
      )
      .then(function () {
        state.busyRedrive = false;
        renderDelivery();
      });
  }

  document.addEventListener('click', function (e) {
    var el = e.target;
    // Tab bar + inline "open tab" links (any tab's markup may carry these).
    var tabBtn = el && el.closest ? el.closest('[data-tab]') : null;
    if (tabBtn) {
      switchTab(tabBtn.getAttribute('data-tab'));
      return;
    }
    var tabGo = el && el.closest ? el.closest('[data-tabgo]') : null;
    if (tabGo) {
      switchTab(tabGo.getAttribute('data-tabgo'));
      return;
    }
    // Links to the dashboard's own pages (any tab's markup may carry these).
    var navTo = el && el.closest ? el.closest('[data-nav]') : null;
    if (navTo) {
      if (canNavigate) {
        HOST.navigate(navTo.getAttribute('data-nav')).catch(function (err) {
          HOST.toast('error', errMsg(err));
        });
      }
      return;
    }
    // Overview action buttons.
    var btn = el && el.closest ? el.closest('[data-act]') : null;
    if (!btn || btn.disabled) return;
    var act = btn.getAttribute('data-act');
    var addr = btn.getAttribute('data-addr');
    if (act === 'phone-connect') phoneAct('phone.connect', addr);
    else if (act === 'phone-disconnect') {
      // With a call on the line (or none that can be told) disconnecting
      // takes it off the receptionist: asked first, by the phone's row.
      if (!disconnectWarning()) {
        phoneAct('phone.disconnect', addr);
        return;
      }
      state.disconnectArm = addr;
      renderPhones();
      focusIn($('phones-body'), '[data-act="phone-disconnect-cancel"]');
    } else if (act === 'phone-disconnect-confirm') {
      state.disconnectArm = null;
      phoneAct('phone.disconnect', addr);
    } else if (act === 'phone-disconnect-cancel') {
      state.disconnectArm = null;
      renderPhones();
      focusIn($('phones-body'), '[data-act="phone-disconnect"][data-addr="' + String(addr || '').replace(/["\\]/g, '\\$&') + '"]');
    } else if (act === 'call-answer') callAct('call.answer');
    else if (act === 'call-reject') callAct('call.reject');
    else if (act === 'call-hangup') callAct('call.hangup');
    else if (act === 'redrive') redriveDead();
  });

  /** Move focus to `sel` in `container`, where an inline confirm opened or
   *  closed (its markup was drawn anew, which drops focus). */
  function focusIn(container, sel) {
    var el = container && container.querySelector ? container.querySelector(sel) : null;
    if (el && typeof el.focus === 'function') el.focus();
  }

  $('speak-send').addEventListener('click', sendSpeech);
  $('speak-input').addEventListener('keydown', function (e) {
    if (e.key === 'Enter') sendSpeech();
  });

  // ---- live events (instant refresh; polling remains the safety net) ------

  // Every name below is declared in manifest.json "events" — the host refuses
  // undeclared subscriptions. The list covers what EVERY tab reacts to; the
  // frame is forwarded to the active tab's onEvent (Overview routes below).
  var SUBSCRIBED_EVENTS = [
    'aokie.dongle.detected',
    'aokie.dongle.driver_required',
    'aokie.dongle.ready',
    'aokie.dongle.error',
    'aokie.phone.pairing_started',
    'aokie.phone.pairing_confirm_required',
    'aokie.phone.connected',
    'aokie.phone.disconnected',
    'aokie.phone.paired',
    'aokie.call.incoming',
    'aokie.call.outbound.dialing',
    'aokie.call.waiting',
    'aokie.call.answered',
    'aokie.call.caller_id',
    'aokie.call.rejected',
    'aokie.call.ended',
  ];

  var eventHandle = null;

  function routeOverviewEvent(name) {
    if (name.indexOf('aokie.dongle.') === 0) {
      refreshPhoneStatus();
      refreshDiag();
      return;
    }
    if (name.indexOf('aokie.phone.') === 0) {
      refreshPhones();
      refreshPhoneStatus();
      return;
    }
    if (name.indexOf('aokie.call.') === 0) {
      refreshCall();
      refreshSwitchboard();
    }
  }

  HOST.events
    .subscribe(SUBSCRIBED_EVENTS, function (evt) {
      var name = (evt && evt.name) || '';
      if (activeTab === 'overview') {
        // Setup mode never shows the Overview, so it never refreshes it.
        if (setupMode === 'off') routeOverviewEvent(name);
        return;
      }
      if (activeDef && typeof activeDef.onEvent === 'function') {
        try {
          activeDef.onEvent(evt);
        } catch (e) {
          /* a tab's event handler must never kill the feed */
        }
      }
    })
    .then(function (h) {
      eventHandle = h;
    })
    .catch(function (e) {
      // The polls cover everything the feed would tell us — say so once.
      HOST.toast('info', 'Live updates are unavailable (' + errMsg(e) + '), so this screen checks every few seconds instead.');
    });

  // ---- lifecycle ----------------------------------------------------------

  document.addEventListener('visibilitychange', function () {
    // Overview's timers only run while Overview is the active tab; other
    // tabs' own intervals check document.hidden themselves.
    if (document.hidden) stopTimers();
    else if (activeTab === 'overview' && setupMode === 'off') startTimers();
  });

  window.addEventListener('pagehide', function () {
    stopTimers();
    if (activeDef && typeof activeDef.unmount === 'function') {
      try {
        activeDef.unmount();
      } catch (e) {
        /* the document is going away */
      }
      activeDef = null;
    }
    if (eventHandle) {
      try {
        eventHandle.unsubscribe();
      } catch (e) {
        /* the document is going away */
      }
      eventHandle = null;
    }
  });

  // The tiles at once, each "Checking…", rather than an empty status card.
  if (!SETUP) renderReadiness();
  if (SETUP) startSetup();
  else if (!document.hidden) startTimers();
})();
