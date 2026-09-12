/*
 * Aokie AI Receptionist — plugin-shipped desktop console (manifest v2
 * ui.screens). CORE module: boot, shared helpers, the tab registry/switcher,
 * the declared-events subscription, and the Overview tab.
 *
 * Runs inside FormLogic Desktop's sandboxed plugin-screen iframe. The host
 * injects `window.PluginHost` (postMessage RPC) before this file executes;
 * there is NO network, NO framework and NO build step here — plain DOM.
 *
 * The host CONCATENATES every .js file in manifest files-list order with
 * '\n;' separators — this file runs FIRST and defines `window.__aokieTabs`
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

  function errMsg(e) {
    return e && e.message ? e.message : String(e);
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

  // ---- tab registry -------------------------------------------------------
  // Each tabs/*.js module registers { mount(el), unmount()?, onEvent(frame)? }.
  // The Overview tab is owned by THIS file (static markup + the timers below);
  // switching mounts/unmounts the registered module for every other tab.
  // Tab state is in-memory only (the sandbox has no location.hash).

  var tabRegistry = {};
  var activeTab = 'overview';
  var activeDef = null; // the mounted module for a non-overview tab

  window.__aokieTabs = {
    register: function (id, def) {
      tabRegistry[id] = def || {};
    },
    /** Shared helpers for tab modules (defined above in this file). */
    util: { esc: esc, errMsg: errMsg, setHtml: setHtml, svg: svg, icons: ICONS },
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
        return transportMode === 'dongle';
      },
      update: function (settingsBag) {
        setTransportFromSettings(settingsBag);
      },
    },
  };

  // ---- transport mode (dongle vs built-in Windows Bluetooth) --------------
  // `dongle` = the original WinUSB-driver radio; `native`/`auto` = the
  // built-in Windows Bluetooth transport, where no WinUSB dongle is involved
  // at all. Native/auto therefore hides every dongle-only surface — the
  // Dongle tab here; the Settings tab gates its own dongle-only Advanced
  // fields, and the Phone tab adjusts its pairing instructions. The mode
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
    var showDongle = transportMode === 'dongle';
    var btns = document.querySelectorAll('.rcp-tab-btn');
    for (var i = 0; i < btns.length; i++) {
      if (btns[i].getAttribute('data-tab') === 'dongle') btns[i].hidden = !showDongle;
    }
    if (!showDongle && activeTab === 'dongle') switchTab('overview');
  }

  function tabContainer(id) {
    return $('tab-' + id);
  }

  function switchTab(id) {
    if (!id || id === activeTab || !tabContainer(id)) return;
    // A tab hidden by the transport mode stays unreachable, even
    // programmatically (data-tabgo links, switchTo).
    if (id === 'dongle' && transportMode !== 'dongle') return;

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
      }
    } else {
      el.innerHTML =
        '<p class="rcp-error">This tab did not register — the screen bundle may be incomplete.</p>';
    }
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
    busyCall: false,
    busyPhones: {}, // address -> true while connect/disconnect runs
    busyRedrive: false,
    now: Date.now(),
  };

  // Event-only view: bounded to one call; durable delivery stays in the outbox.
  var conversation = { callId: '', turns: [] };
  var conversationStartedAt = 0;
  function renderConversation() {
    var body = $('conversation-body');
    var following = body.scrollHeight - body.scrollTop - body.clientHeight < 48;
    var html = conversation.turns.map(function (turn) {
      var labels = [];
      if (turn.overlapped) labels.push('Spoke while Aokie was talking');
      if (turn.delivery === 'interrupted') labels.push('Reply interrupted');
      if (turn.delivery === 'error') labels.push('Speech delivery failed');
      if (turn.delivery === 'operator_ended') labels.push('Stopped by operator');
      if (turn.kind === 'control') labels.push('Conversation control');
      if (turn.corrected) labels.push('Transcript corrected');
      return '<article class="rcp-conversation__turn"><strong>' +
        (turn.speaker === 'bot' ? 'Aokie' : 'Caller') + '</strong>' +
        (labels.length ? '<small>' + esc(labels.join(' · ')) + '</small>' : '') +
        '<p>' + esc(turn.text) + '</p></article>';
    }).join('');
    setHtml(body, html || '<p class="rcp-empty">Waiting for finalized speech. Live call transcription must be enabled.</p>');
    if (following) body.scrollTop = body.scrollHeight;
  }

  function collectConversation(evt) {
    var name = evt.name || '';
    var data = evt.data || {};
    var callId = data.callId || evt.correlationId;
    if (typeof callId !== 'string' || !callId) return;
    if (name === 'aokie.call.incoming' || name === 'aokie.call.outbound.dialing' || name === 'aokie.call.answered') {
      if (conversation.callId !== callId) {
        var startedAt = Date.parse(evt.occurredAt || data.at || '');
        if (Number.isFinite(startedAt) && startedAt <= conversationStartedAt) return;
        if (Number.isFinite(startedAt)) conversationStartedAt = startedAt;
        conversation = { callId: callId, turns: [] };
        renderConversation();
      }
      return;
    }
    if (name !== 'aokie.call.turn.final' && name !== 'aokie.call.turn.corrected') return;
    if (conversation.callId && conversation.callId !== callId) return;
    if (!Number.isInteger(data.turn) || data.turn < 0 || typeof data.text !== 'string') return;
    conversation.callId = callId;
    var prior = conversation.turns.find(function (turn) { return turn.turn === data.turn; });
    if (prior && prior.corrected && name === 'aokie.call.turn.final') return;
    var turn = Object.assign({}, prior || {}, {
      turn: data.turn,
      speaker: data.speaker || (prior && prior.speaker) || 'caller',
      text: data.text.slice(0, 12000),
      at: (prior && prior.at) || data.at || evt.occurredAt,
      overlapped: data.overlapped === true || !!(prior && prior.overlapped),
      delivery: data.delivery || (prior && prior.delivery),
      kind: data.kind || (prior && prior.kind),
      corrected: name === 'aokie.call.turn.corrected',
    });
    if (prior) conversation.turns[conversation.turns.indexOf(prior)] = turn;
    else conversation.turns.push(turn);
    conversation.turns.sort(function (a, b) {
      var delta = Date.parse(a.at) - Date.parse(b.at);
      return Number.isFinite(delta) && delta !== 0 ? delta : a.turn - b.turn;
    });
    if (conversation.turns.length > 120) conversation.turns.splice(0, conversation.turns.length - 120);
    renderConversation();
  }

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
    ).then(renderReadiness);
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
    ).then(renderPhones);
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
    ).then(renderLive);
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
      renderLive(); // agent-mode gates the operator composer
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

  function slowTick() {
    refreshSnapshot();
    refreshPhoneStatus();
    refreshDiag();
    refreshPhones();
    if (slowTicks % 6 === 0) refreshSettings(); // every ~30 s (and at start)
    slowTicks += 1;
  }

  function fastTick() {
    state.now = Date.now();
    refreshCall();
    refreshSwitchboard();
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

  function pluginIsLive(snap) {
    return !!(snap && (snap.state === 'running' || snap.state === 'unhealthy'));
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

    if (!running) {
      headline.textContent = 'Aokie is not running';
      sub.textContent = snap.reason || 'Start the plugin from the Plugins workspace to take calls.';
      setPill(pill, pillText, snap.state === 'crashed' ? 'is-err' : 'is-neutral', snap.state || 'Stopped');
    } else if (snap.lastHealthError || (snap.state === 'unhealthy' && !health)) {
      headline.textContent = 'Running — needs attention';
      sub.textContent = snap.lastHealthError || snap.reason || 'The plugin is running, but its health could not be verified.';
      setPill(pill, pillText, 'is-warn', 'Needs attention');
    } else if (!health) {
      headline.textContent = 'Starting up';
      sub.textContent = 'No health report from the plugin yet.';
      setPill(pill, pillText, 'is-neutral', 'Starting');
    } else if (status === 'ok') {
      headline.textContent = 'Ready for calls';
      sub.textContent = health.detail || 'The phone bridge and voice pipeline are healthy.';
      setPill(pill, pillText, 'is-ok', 'Healthy');
    } else if (status === 'degraded') {
      headline.textContent = 'Running — needs attention';
      sub.textContent = health.detail || 'The plugin reports a degraded component.';
      setPill(pill, pillText, 'is-warn', 'Degraded');
    } else {
      headline.textContent = 'Plugin health: ' + (status || 'unknown');
      sub.textContent = health.detail || 'See the readiness checks below.';
      setPill(pill, pillText, status === 'error' ? 'is-err' : 'is-warn', status || 'Unknown');
    }

    var pluginVersion = snap.version || (snap.manifest && snap.manifest.version);
    version.textContent = pluginVersion ? 'Aokie v' + pluginVersion : '';
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
      if (phone === undefined) {
        items.push({ icon: ICONS.smartphone, label: 'Phone bridge', value: '…', note: 'checking phone.status', ok: null });
        return;
      }
      if (phone === null) {
        items.push({ icon: ICONS.smartphone, label: 'Phone bridge', value: 'Unavailable', note: state.phoneError || 'phone.status failed', ok: false });
        return;
      }
      var linked = !!phone.connected && !phone.error && !phone.pairingConfirm;
      var value = phone.error ? 'Needs attention' : phone.pairingConfirm ? 'Confirm pairing' : linked ? 'Bluetooth linked' : phone.paired ? 'Paired, offline' : 'Not paired';
      var device = phone.device || null;
      var note =
        phone.error ||
        (device && (device.name || device.address)) ||
        'pair a phone with "Aokie AI Assistant"';
      items.push({ icon: ICONS.smartphone, label: 'Phone bridge', value: value, note: note, ok: linked });
    })();

    // AI responder (plugin health components)
    (function () {
      var responder = health && health.components && health.components.responder;
      if (!running) {
        items.push({ icon: ICONS.server, label: 'AI responder', value: '—', note: 'plugin not running', ok: null });
        return;
      }
      if (snap.lastHealthError || (snap.state === 'unhealthy' && !health)) {
        items.push({ icon: ICONS.server, label: 'AI responder', value: 'Unavailable', note: snap.lastHealthError || snap.reason || 'health check unavailable', ok: false });
        return;
      }
      if (!responder) {
        items.push({ icon: ICONS.server, label: 'AI responder', value: '…', note: health ? 'no responder details in the latest health report' : 'no health report yet', ok: null });
        return;
      }
      var agent = responder.mode === 'agent';
      var value = agent ? (responder.ready ? 'LLM ready' : 'LLM down') : 'Flow replies';
      var note =
        responder.llmError ||
        responder.note ||
        (agent ? 'local LLM answering' : 'replies come from FormLogic flows');
      items.push({ icon: ICONS.server, label: 'AI responder', value: value, note: note, ok: responder.ready !== false });
    })();

    // Radio & dongle (dongle.diagnostics)
    (function () {
      if (diag === undefined) {
        items.push({ icon: ICONS.radio, label: 'Radio & dongle', value: '…', note: 'checking diagnostics', ok: null });
        return;
      }
      var radio = diag && diag.radio;
      if (!radio) {
        items.push({ icon: ICONS.radio, label: 'Radio & dongle', value: 'Unavailable', note: state.diagError || 'no radio diagnostics', ok: false });
        return;
      }
      var consentPaused = !!radio.paused && radio.blockedBy === 'consent';
      var voiceErr = radio.voiceSttError || radio.voiceTtsError || '';
      var stale = Number(radio.staleSttResults) || 0;
      var value = consentPaused ? 'Paused for consent' : radio.initialized ? 'Ready' : 'Not initialised';
      var note =
        (consentPaused ? radio.reason : '') ||
        radio.error ||
        voiceErr ||
        (radio.initialized
          ? 'radio up' + (stale > 0 ? ' · ' + stale + ' stale STT result' + (stale === 1 ? '' : 's') : '')
          : 'connect a supported dongle');
      items.push({
        icon: ICONS.radio,
        label: 'Radio & dongle',
        value: value,
        note: note,
        ok: !consentPaused && !!radio.initialized && !radio.error && !voiceErr,
      });
    })();

    // Data delivery (outbox counts)
    (function () {
      if (diag === undefined) {
        items.push({ icon: ICONS.cloud, label: 'Data delivery', value: '…', note: 'checking outbox', ok: null });
        return;
      }
      var outbox = diag && diag.outbox;
      if (!outbox) {
        items.push({ icon: ICONS.cloud, label: 'Data delivery', value: 'Unknown', note: state.diagError || 'no outbox counts', ok: null });
        return;
      }
      var pending = Number(outbox.pending) || 0;
      var failed = Number(outbox.failed) || 0;
      var dead = Number(outbox.dead) || 0;
      var healthy = dead === 0 && failed === 0;
      items.push({
        icon: ICONS.cloud,
        label: 'Data delivery',
        value: healthy ? (pending > 0 ? 'Delivering…' : 'All delivered') : 'Needs attention',
        note: pending + ' pending · ' + failed + ' failed · ' + dead + ' dead',
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
        return (
          '<div class="rcp-ready">' +
          '<span class="rcp-ready__icon">' + it.icon + '</span>' +
          '<small>' + esc(it.label) + '</small>' +
          '<strong>' + esc(it.value) + '</strong>' +
          '<p title="' + esc(it.note) + '">' + esc(it.note) + '</p>' +
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
      setHtml(body, '<p class="rcp-loading">Loading…</p>');
      return;
    }

    if (!call) {
      pill.hidden = true;
      speakRow.hidden = true;
      if (!state.callKnown) {
        title.textContent = 'Live call unavailable';
        setHtml(body, '<p class="rcp-error">' + esc(state.callError || 'call.current failed.') + '</p>');
        return;
      }
      if (state.callPauseReason) {
        title.textContent = 'Receptionist paused';
        setHtml(
          body,
          '<div class="rcp-empty-call">' +
            '<span class="rcp-empty-call__icon">' + ICONS.alert + '</span>' +
            '<h4>Consent review required</h4>' +
            '<p>' + esc(state.callPauseReason) + '</p>' +
            '<button type="button" class="rcp-link-btn" data-tabgo="consent">Review consent</button>' +
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
          '<p>When a call rings on the paired phone it appears here with answer, reject and hang-up controls.</p>' +
          '</div>'
      );
      return;
    }

    var caller = call.from || 'Unknown caller';
    var outbound = call.direction === 'outbound';
    title.textContent = caller;

    if (call.state === 'ringing') {
      setPill(pill, pillText, 'is-warn', outbound ? 'Dialing' : 'Ringing');
      speakRow.hidden = true;
      setHtml(
        body,
        '<div class="rcp-ringing">' +
          '<small>' + (outbound ? 'OUTBOUND CALL' : 'INCOMING CALL') + '</small>' +
          '<h4>' + esc(caller) + '</h4>' +
          (outbound ? '<p>Waiting for the other person to answer.</p>' : '') +
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
    setPill(pill, pillText, 'is-live', callDuration(call) || 'Live call');
    var owner = replyOwner();
    var note =
      owner === 'agent'
        ? 'The AI receptionist owns this conversation — operator speech is muted'
        : owner === 'operator'
          ? 'Operator mode — you can speak to the caller'
          : 'Checking who owns replies…';
    setHtml(
      body,
      '<div class="rcp-active">' +
        '<small>' + (outbound ? 'OUTBOUND CALL' : 'INCOMING CALL') + '</small>' +
        '<span class="rcp-active__note">' + esc(note) + '</span>' +
        '<button type="button" class="rcp-call-action is-reject" data-act="call-hangup"' + (state.busyCall ? ' disabled' : '') + '>' +
        ICONS.phone + ' Hang up</button>' +
        '</div>'
    );
    speakRow.hidden = false;
    var muted = owner !== 'operator';
    input.disabled = state.busyCall || muted;
    input.placeholder = muted ? 'Disabled while the AI receptionist replies' : 'Say something to the caller…';
    send.disabled = state.busyCall || muted;
  }

  // ---- delivery -----------------------------------------------------------

  function renderSwitchboard() {
    var body = $('switchboard-body');
    var board = state.switchboard;
    body.hidden = board === undefined || !!(board && !board.waiting && !board.parked && !board.switchInProgress);
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
      title.textContent = 'Loading…';
      setPill(pill, pillText, 'is-neutral', 'Checking');
      setHtml(body, '<p class="rcp-loading">Loading…</p>');
      return;
    }

    var outbox = diag && diag.outbox;
    if (!outbox) {
      title.textContent = 'Outbox unavailable';
      setPill(pill, pillText, 'is-neutral', 'Unknown');
      setHtml(body, '<p class="rcp-error">' + esc(state.diagError || 'dongle.diagnostics returned no outbox counts.') + '</p>');
      return;
    }

    var pending = Number(outbox.pending) || 0;
    var failed = Number(outbox.failed) || 0;
    var dead = Number(outbox.dead) || 0;
    var collisions = Number(outbox.keyCollisions) || 0;
    var healthy = dead === 0 && failed === 0;

    title.textContent = healthy
      ? pending > 0
        ? 'Delivering…'
        : 'All events delivered'
      : 'Delivery needs attention';
    if (healthy) setPill(pill, pillText, 'is-ok', 'Healthy');
    else if (dead > 0) setPill(pill, pillText, 'is-warn', dead + ' dead');
    else setPill(pill, pillText, 'is-warn', failed + ' failed');

    var html =
      '<div class="rcp-metrics">' +
      '<span><strong>' + pending + '</strong><small>Pending</small></span>' +
      '<span><strong>' + failed + '</strong><small>Failed</small></span>' +
      '<span><strong>' + dead + '</strong><small>Dead</small></span>' +
      '</div>';
    if (collisions > 0) {
      html +=
        '<p class="rcp-delivery-note is-warn">' +
        collisions + ' idempotency key collision' + (collisions === 1 ? '' : 's') +
        ' rejected — a key-derivation bug; check the plugin logs.</p>';
    }
    if (dead > 0) {
      html +=
        '<div class="rcp-delivery-actions">' +
        '<button type="button" class="rcp-button" data-act="redrive"' + (state.busyRedrive ? ' disabled' : '') + '>' +
        (state.busyRedrive
          ? 'Redriving…'
          : 'Redrive ' + dead + ' dead event' + (dead === 1 ? '' : 's')) +
        '</button></div>';
    }
    setHtml(body, html);
  }

  // ---- paired phones ------------------------------------------------------

  function renderPhones() {
    var title = $('phones-title');
    var body = $('phones-body');
    var phones = state.phones;

    if (phones === undefined) {
      title.textContent = 'Loading…';
      setHtml(body, '<p class="rcp-loading">Loading…</p>');
      return;
    }
    if (phones === null) {
      title.textContent = 'Paired phones unavailable';
      setHtml(body, '<p class="rcp-error">' + esc(state.phonesError || 'phone.listPaired failed.') + '</p>');
      return;
    }
    if (phones.length === 0) {
      title.textContent = 'No phones paired';
      setHtml(
        body,
        '<p class="rcp-loading">No phones are bonded yet. Open the ' +
          '<button type="button" class="rcp-link-btn" data-tabgo="phone">Phone setup tab</button>' +
          ' to pair one.</p>'
      );
      return;
    }

    title.textContent = phones.length + ' phone' + (phones.length === 1 ? '' : 's') + ' bonded';
    var html =
      '<div class="rcp-phone-list">' +
      phones
        .map(function (d) {
          var addr = String(d.address || '');
          var busy = !!state.busyPhones[addr];
          var connected = !!d.connected;
          return (
            '<div class="rcp-phone-row">' +
            '<span class="rcp-phone-row__id">' +
            '<strong>' + esc(d.name || 'Unknown phone') + '</strong>' +
            '<small>' + esc(addr) + '</small>' +
            '</span>' +
            '<span class="rcp-pill ' + (connected ? 'is-ok' : 'is-neutral') + '"><i></i>' +
            (connected ? 'Connected' : 'Offline') +
            '</span>' +
            '<button type="button" class="rcp-button" data-act="' +
            (connected ? 'phone-disconnect' : 'phone-connect') +
            '" data-addr="' + esc(addr) + '"' + (busy ? ' disabled' : '') + '>' +
            (busy
              ? connected
                ? 'Disconnecting…'
                : 'Reconnecting…'
              : connected
                ? 'Disconnect'
                : 'Reconnect') +
            '</button>' +
            '</div>'
          );
        })
        .join('') +
      '</div>';
    setHtml(body, html);
  }

  // ---- settings summary (read-only) ---------------------------------------

  function engineLabel(v) {
    if (v === 'sherpa') return 'Sherpa (Piper/VITS voices)';
    if (v === 'pocket') return 'Pocket-TTS';
    if (!v) return 'Pocket-TTS (default)';
    return String(v);
  }

  function renderSettings() {
    var title = $('settings-title');
    var body = $('settings-body');
    var s = state.settings;

    if (s === undefined) {
      title.textContent = 'Loading…';
      setHtml(body, '<p class="rcp-loading">Loading…</p>');
      return;
    }
    if (s === null) {
      title.textContent = 'Settings unavailable';
      setHtml(body, '<p class="rcp-error">' + esc(state.settingsError || 'settings.get failed.') + '</p>');
      return;
    }

    var bag = s.settings || {};
    var agent = !!bag.aiReceptionist;
    var greetingSet = typeof bag.greeting === 'string' && bag.greeting.trim() !== '';
    var voice = typeof bag.ttsVoice === 'string' && bag.ttsVoice !== '' ? bag.ttsVoice : 'Default';

    title.textContent = agent ? 'AI receptionist answers calls' : 'Flow-driven replies';

    var rows = [
      ['Greeting', greetingSet ? 'Custom greeting set' : 'Default greeting'],
      ['Voice engine', engineLabel(bag.ttsEngine)],
      ['Voice', voice],
      ['Agent mode', agent ? 'On — the AI answers' : 'Off — flows reply'],
      ['Listen during replies', bag.bargeIn ? 'On — captures overlap and allows interruptions' : 'Off — caller recognition pauses'],
      ['Config version', s.configVersion != null ? 'v' + s.configVersion : '—'],
    ];
    var html =
      '<div class="rcp-settings-list">' +
      rows
        .map(function (r) {
          return (
            '<div class="rcp-setting"><small>' + esc(r[0]) + '</small>' +
            '<span title="' + esc(r[1]) + '">' + esc(r[1]) + '</span></div>'
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
          if (msg.indexOf('stale_call') !== -1) {
            HOST.toast('info', 'The call changed — refreshing the live state before more actions.');
            return refreshCall().then(function () {
              return false;
            });
          }
          HOST.toast('error', 'Call control failed: ' + msg);
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
              ? 'Reconnect requested for ' + address + ' — watching for the phone to come back.'
              : 'Disconnect requested for ' + address + ' — the phone may take a moment to drop.'
          );
          return refreshPhones().then(refreshPhoneStatus);
        },
        function (e) {
          HOST.toast(
            'error',
            (command === 'phone.connect' ? 'Reconnect failed: ' : 'Disconnect failed: ') + errMsg(e)
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
              ? 'Redriving ' + revived + ' dead event' + (revived === 1 ? '' : 's') +
                ' — they re-enter the delivery pipeline now.'
              : 'Nothing to redrive — no dead events left.'
          );
          return refreshDiag();
        },
        function (e) {
          HOST.toast('error', 'Redrive failed: ' + errMsg(e));
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
    // Overview action buttons.
    var btn = el && el.closest ? el.closest('[data-act]') : null;
    if (!btn || btn.disabled) return;
    var act = btn.getAttribute('data-act');
    if (act === 'phone-connect') phoneAct('phone.connect', btn.getAttribute('data-addr'));
    else if (act === 'phone-disconnect') phoneAct('phone.disconnect', btn.getAttribute('data-addr'));
    else if (act === 'call-answer') callAct('call.answer');
    else if (act === 'call-reject') callAct('call.reject');
    else if (act === 'call-hangup') callAct('call.hangup');
    else if (act === 'redrive') redriveDead();
    else if (act === 'conversation-clear') { conversation.turns = []; renderConversation(); }
  });

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
    'aokie.call.turn.final',
    'aokie.call.turn.corrected',
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
      if (evt) collectConversation(evt);
      if (name.indexOf('aokie.call.turn.') === 0) return;
      if (activeTab === 'overview') {
        routeOverviewEvent(name);
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
      HOST.toast('info', 'Live events unavailable (' + errMsg(e) + ') — falling back to polling.');
      $('conversation-note').textContent = 'Live transcript unavailable: the event subscription failed. Other status cards continue polling.';
    });

  // ---- lifecycle ----------------------------------------------------------

  document.addEventListener('visibilitychange', function () {
    // Overview's timers only run while Overview is the active tab; other
    // tabs' own intervals check document.hidden themselves.
    if (document.hidden) stopTimers();
    else if (activeTab === 'overview') startTimers();
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

  if (!document.hidden) startTimers();
})();
