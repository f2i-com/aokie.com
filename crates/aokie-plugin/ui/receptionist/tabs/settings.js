/*
 * Settings tab — the editable form over Aokie's live connector settings.
 *
 * Started as a port of FormLogic Desktop's compiled AokieSettingsForm
 * (desktop/src/aokie/AokieCard.tsx + desktop/src/aokie/aokieSettings.ts).
 * Under OAIY, calls go to OAIY Desktop's voice gateway: the Front desk agent
 * writes the replies and OAIY Voice hears and speaks. So this form no longer
 * offers Aokie's own speech engines, voices, model or endpoint pickers — the
 * call voice, the model and the replies are set in OAIY, and the form says
 * where. What Aokie still owns is here: whether it answers, the greeting and
 * brief it sends with each call, who is answered and the hardware. Aokie has
 * no speech of its own, so nothing here needs it: a call that does not go to
 * OAIY is answered by nobody (it rings through), and the form says to send
 * calls to OAIY.
 *
 * Reads via `settings.get`, writes ONLY the fields the operator changed via
 * `settings.set` (the plugin merges per key, so untouched safety keys like
 * autoAnswer and unknown/newer plugin keys are never rewritten by an
 * unrelated edit — audit AOK-SAFE-001). Keys the form no longer shows keep
 * their saved values.
 */
(function () {
  'use strict';

  var HOST = window.PluginHost;
  var TABS = window.__aokieTabs;
  if (!HOST || !TABS) return;
  var U = TABS.util;
  var esc = U.esc;
  var errMsg = U.errMsg;
  var OAIY = TABS.oaiy;

  // ======================================================================
  // Settings bag helpers (from desktop/src/aokie/aokieSettings.ts).
  // ======================================================================

  // Every key the plugin reports, including the ones this form no longer
  // shows: the dirty-diff below compares them all, so a hidden key can never
  // be written by accident.
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
    realtimeVoiceMode: 'legacy',
    realtimeVoiceEndpoint: '',
    realtimeVoiceDestination: '',
    realtimeVoice: 'marin',
    realtimeTurnDetection: 'server_vad',
    realtimeMaxOutputTokens: 384,
    sttEndpoint: '',
    ttsEndpoint: '',
    sttEndpointMs: 450,
    bargeIn: false,
    conversationAcknowledgements: false,
    sendAudio: false,
    bargeSensitivity: 650,
    hfpCodec: 'auto',
    transportMode: 'dongle',
    reenumerateHwid: '',
    legacyPairingPin: false,
    agentHangup: false,
    allowedNumbers: '',
    callsAllowedOnly: false,
    otherCallers: 'ring',
    textsAllowedOnly: false,
    rejectPrivate: false,
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
      realtimeVoiceMode:
        src.realtimeVoiceMode === 'desktop_realtime' || src.realtimeVoiceMode === 'legacy'
          ? src.realtimeVoiceMode
          : d.realtimeVoiceMode,
      realtimeVoiceEndpoint:
        typeof src.realtimeVoiceEndpoint === 'string'
          ? src.realtimeVoiceEndpoint
          : d.realtimeVoiceEndpoint,
      realtimeVoiceDestination:
        typeof src.realtimeVoiceDestination === 'string'
          ? src.realtimeVoiceDestination
          : d.realtimeVoiceDestination,
      realtimeVoice:
        ['marin', 'cedar', 'alloy', 'ash', 'ballad', 'coral', 'echo', 'sage', 'shimmer', 'verse']
          .indexOf(src.realtimeVoice) !== -1
          ? src.realtimeVoice
          : d.realtimeVoice,
      realtimeTurnDetection:
        src.realtimeTurnDetection === 'semantic_vad' || src.realtimeTurnDetection === 'server_vad'
          ? src.realtimeTurnDetection
          : d.realtimeTurnDetection,
      realtimeMaxOutputTokens:
        typeof src.realtimeMaxOutputTokens === 'number' && isFinite(src.realtimeMaxOutputTokens)
          ? src.realtimeMaxOutputTokens
          : d.realtimeMaxOutputTokens,
      sttEndpoint: typeof src.sttEndpoint === 'string' ? src.sttEndpoint : d.sttEndpoint,
      ttsEndpoint: typeof src.ttsEndpoint === 'string' ? src.ttsEndpoint : d.ttsEndpoint,
      sttEndpointMs:
        typeof src.sttEndpointMs === 'number' && isFinite(src.sttEndpointMs)
          ? src.sttEndpointMs
          : d.sttEndpointMs,
      bargeIn: boolSetting(src.bargeIn, d.bargeIn),
      conversationAcknowledgements: boolSetting(src.conversationAcknowledgements, d.conversationAcknowledgements),
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
      agentHangup: boolSetting(src.agentHangup, d.agentHangup),
      allowedNumbers: typeof src.allowedNumbers === 'string' ? src.allowedNumbers : d.allowedNumbers,
      callsAllowedOnly: boolSetting(src.callsAllowedOnly, d.callsAllowedOnly),
      otherCallers: src.otherCallers === 'reject' || src.otherCallers === 'ring' ? src.otherCallers : d.otherCallers,
      textsAllowedOnly: boolSetting(src.textsAllowedOnly, d.textsAllowedOnly),
      rejectPrivate: boolSetting(src.rejectPrivate, d.rejectPrivate),
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

  var AOKIE_CODEC_OPTIONS = [
    { value: 'auto', label: 'Auto' },
    { value: 'cvsd', label: 'CVSD (8kHz)' },
    { value: 'wbs', label: 'mSBC (16kHz, wideband)' },
  ];

  /** Where each part of a call is set in OAIY Desktop: [label, page, the
   *  page's name, the rest of the way there]. The page name is a link to it;
   *  live values (the voice, the model) follow from oaiyStatus. */
  var SET_IN_OAIY = [
    ['Replies', 'agent', 'Agent', ' › Front desk project: /brief.md and the knowledge files'],
    ['Call and text instructions', 'agent', 'Agent', ' › Phone (the phone chip at the top): Answer phone calls, and your instructions for calls and for texts'],
    // Missed-call callbacks and call screening (who is answered, blocked
    // numbers) are set there; they write Aokie's own screening and outbound
    // keys. The allowed list (and hidden numbers, which go with it) is also
    // here, in "Who is answered": it is what to set while trying Aokie out.
    ['Callbacks and screening', 'agent', 'Agent', ' › Phone: missed-call callbacks, who is answered, and blocked numbers'],
    ['Speech', 'services', 'Services', ': speech to text, and the call voice'],
    ['Model', 'engines', 'Engines', ', which the Front desk agent’s OAIY provider follows (Agent › Settings › AI providers)'],
    // (both on Calendar's Hours & Services page: OAIY's `hours` view)
    ['Voice on calls', 'hours', 'Hours & Services', ' (Calendar) › How the phone sounds'],
    ['Hours, services and booking', 'hours', 'Hours & Services', ' (Calendar); the receptionist looks these up during calls'],
  ];

  /** The plugin's refusals name settings by their keys ("persona must be at
   *  most 4000 characters"): said with the form's own names for them. */
  var FIELD_LABELS = {
    greeting: 'The greeting',
    persona: 'The receptionist brief',
    allowedNumbers: 'The allowed numbers',
    callsAllowedOnly: '“Answer calls only from these numbers”',
    otherCallers: '“Calls from other numbers”',
    textsAllowedOnly: '“Text only these numbers”',
    rejectPrivate: '“Do not answer hidden or private numbers”',
    autoAnswer: '“Auto-answer incoming calls”',
    agentHangup: '“The receptionist can end the call”',
    hfpCodec: 'The Bluetooth audio codec',
    reenumerateHwid: 'The hardware id to re-enumerate',
    legacyPairingPin: 'Legacy PIN pairing',
    answerTone: 'The test tone',
    aiReceptionist: 'The AI receptionist',
    realtimeVoiceMode: 'Where calls go',
    realtimeVoiceEndpoint: 'Where calls go',
    realtimeVoiceDestination: 'Where calls go',
  };

  /** A refusal with the key it starts with (or names before a colon) said
   *  as the field's name. */
  function fieldWords(message) {
    var has = function (key) {
      return Object.prototype.hasOwnProperty.call(FIELD_LABELS, key);
    };
    return String(message)
      .replace(/^([a-z][A-Za-z]*)\b/, function (word) {
        return has(word) ? FIELD_LABELS[word] : word;
      })
      .replace(/\b([a-z]+[A-Z][A-Za-z]*):/g, function (match, word) {
        return has(word) ? FIELD_LABELS[word] + ':' : match;
      });
  }

  /** The Windows-only hardware knob (re-enumerating a dongle by its Windows
   *  device path) shows only where the screen runs on Windows. */
  var ON_WINDOWS =
    typeof navigator !== 'undefined' && /\bWin(?:dows|32|64)\b/.test(String(navigator.userAgent || '') + ' ' + String(navigator.platform || ''));

  // ======================================================================
  // Tab state (module-level — survives tab switches, like the compiled
  // form's component state survives collapse/expand).
  // ======================================================================

  var root = null;
  var loaded = false;
  var loading = false;
  var saving = false;
  var error = null;
  // Keys the last save reported as applies-at-restart (settings.set's
  // appliesAtReconnect) — e.g. where calls go. Non-null renders the
  // "Restart receptionist now" apply banner.
  var pendingRestart = null;
  // "Restart receptionist now" is under way (the call check, then the
  // restart): a second press does nothing meanwhile.
  var restarting = false;
  // Reload asked first: it would throw away edits not saved yet.
  var reloadArm = false;
  // The control to give focus back to once a save or a read is over: the
  // form is drawn anew for each (Save is off meanwhile), which drops focus.
  var heldFocus = null;
  var settings = withAokieDefaults(null);
  var baseline = withAokieDefaults(null);
  // The host's AI sources, read for OAIY Voice's running state only.
  // undefined = not read yet; null = the read failed.
  var sources;
  // PluginHost.oaiyStatus: the chosen call voice and the engines' model.
  // null = not available (older host or failed read).
  var oaiyStatus = null;
  // Provenance watch (live report 2026-07-18: "I changed the greeting and it
  // did not take"). A linked FormLogic app re-applies its Receptionist
  // Settings record on EVERY incoming call (the configure-receptionist flow
  // calls settings.set with the greeting and persona among others), so an
  // edit made here is genuinely saved and then genuinely replaced moments
  // later. The plugin bumps configVersion on every write, so a bump this tab
  // did not cause IS an external writer — we watch for it and say so plainly
  // instead of letting the operator conclude the form is broken.
  var configVersion = null;
  var appManaged = false;

  /** True once an external writer has been observed bumping configVersion
   *  AND changing the greeting or the brief. Every settings write bumps the
   *  version (the Consent and Dongle tabs, OAIY's own Agent › Phone page),
   *  so a bump alone said "your FormLogic app re-applied these settings" on
   *  OAIY, which has no such app, for any change at all. */
  function noteConfigVersion(next, ours, greetingOrBriefChanged) {
    if (typeof next !== 'number') return;
    if (configVersion !== null && next !== configVersion && !ours && greetingOrBriefChanged) {
      appManaged = true;
    }
    configVersion = next;
  }

  function hasUnsavedEdits() {
    return Object.keys(settingsPatch(baseline, settings)).length > 0;
  }

  function load(preserveEdits) {
    if (loading) {
      render();
      return;
    }
    loading = true;
    error = null;
    holdFocus();
    render();
    var settingsP = HOST.command('settings.get');
    // Source listing is best-effort — a failure must not block the form.
    var sourcesP =
      typeof HOST.aiSources === 'function'
        ? HOST.aiSources().then(
            function (list) {
              return Array.isArray(list) ? list : [];
            },
            function () {
              return null;
            }
          )
        : Promise.resolve(null);
    return Promise.all([settingsP, sourcesP, OAIY.readStatus()]).then(
      function (results) {
        var data = results[0] || {};
        var merged = withAokieDefaults(data.settings);
        // What the form last had saved, against what is saved now: the
        // greeting or brief changed by someone else is what the notice is for.
        var changedElsewhere =
          loaded && (merged.greeting !== baseline.greeting || merged.persona !== baseline.persona);
        // A tab-entry refresh must not eat edits that were already present or
        // were typed while the two reads were in flight. The working settings
        // + their old baseline stay paired until the operator saves or
        // explicitly presses Reload.
        var keepWorkingCopy = !!preserveEdits && loaded && hasUnsavedEdits();
        sources = results[1];
        oaiyStatus = results[2];
        // ⚠️ baseline and settings must be SEPARATE objects: the form edits
        // MUTATE `settings` in place (plain DOM, not React state-replace),
        // and an aliased baseline would make every dirty-diff empty.
        if (!keepWorkingCopy) {
          baseline = merged;
          settings = withAokieDefaults(merged);
        }
        // Keep the shared transport truth (the Dongle tab's visibility in
        // app.js) in step with the plugin's saved settings.
        if (TABS.transport && TABS.transport.update) TABS.transport.update(merged);
        // A bump between polls that this tab did not cause = the linked app
        // re-applied its record (see the provenance note above).
        noteConfigVersion(data.configVersion, false, changedElsewhere);
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

  /** Save the changed keys. Resolves true when the plugin accepted them. */
  function save() {
    if (saving) return Promise.resolve(false);
    var patch = settingsPatch(baseline, settings);
    if (Object.keys(patch).length === 0) {
      HOST.toast('info', 'Nothing to save: no setting was changed.');
      return Promise.resolve(false);
    }
    // An empty allowed list with "Answer calls only from these numbers" on
    // answers nobody (and with "Reject them", refuses every caller): not
    // saved, and said by the list. (Only when this save is what does it.)
    var empty = emptyListText();
    if (empty && ('allowedNumbers' in patch || 'callsAllowedOnly' in patch || 'otherCallers' in patch)) {
      error = 'Not saved: ' + empty;
      render();
      var list = root && root.querySelector ? root.querySelector('[data-key="allowedNumbers"]') : null;
      if (list) list.focus();
      return Promise.resolve(false);
    }
    return write(patch, null);
  }

  /** Write `patch` to the plugin. `keep`: edits not saved yet that stay in
   *  the form over what comes back (a write of some keys only, as "Send
   *  calls to OAIY" is). Resolves true when the plugin accepted it. */
  function write(patch, keep) {
    if (saving) return Promise.resolve(false);
    saving = true;
    error = null;
    reloadArm = false;
    // (a mouse press on Save leaves no focus behind: Save has it afterwards)
    if (!holdFocus() && U.focusLost(root)) heldFocus = { sel: '#set-save' };
    render();
    // The form as it was sent: what is typed while the save is out is kept
    // over what comes back, not overwritten by it.
    var sent = withAokieDefaults(settings);
    var ok = false;
    return HOST.command('settings.set', patch)
      .then(
        function (data) {
          ok = true;
          var meanwhile = settingsPatch(sent, settings);
          var merged = withAokieDefaults((data || {}).settings);
          // Separate objects — see the aliasing note in load().
          baseline = merged;
          settings = withAokieDefaults(merged);
          [keep, meanwhile].forEach(function (edits) {
            for (var k in edits || {}) {
              if (Object.prototype.hasOwnProperty.call(edits, k)) settings[k] = edits[k];
            }
          });
          // The Overview's summary reads them again when it is next shown.
          if (TABS.settingsChanged) TABS.settingsChanged();
          // The saved mode now drives the Dongle tab's visibility too.
          if (TABS.transport && TABS.transport.update) TABS.transport.update(merged);
          // OUR bump — never mistake a successful save for the linked app.
          noteConfigVersion((data || {}).configVersion, true, false);
          var blocked = data && typeof data.blocked === 'string' ? data.blocked.trim() : '';
          // Start-only keys (where calls go and friends) are saved but NOT
          // live until the plugin restarts — surface the apply step instead
          // of letting the change look silently ignored.
          var pend = (data || {}).appliesAtReconnect;
          pendingRestart = pend && pend.length ? pend.slice() : null;
          if (blocked) {
            HOST.toast(
              'error',
              'Settings saved, but the receptionist is paused: accept the new data destination in Consent before calls can resume.'
            );
          } else if (pendingRestart) {
            HOST.toast(
              'success',
              'Saved. One more step: press Restart receptionist now, beside Save, to apply it.'
            );
          } else {
            HOST.toast('success', 'Saved. It applies from the next call.');
          }
        },
        function (e) {
          error = fieldWords(errMsg(e));
        }
      )
      .then(function () {
        saving = false;
        render();
        return ok;
      });
  }

  var ROUTE_KEYS = ['realtimeVoiceMode', 'realtimeVoiceEndpoint', 'realtimeVoiceDestination', 'aiReceptionist'];

  /** Point calls at OAIY's voice gateway, writing those keys alone: other
   *  edits in the form stay unsaved, for Save (it once saved them too,
   *  unasked). The route keys are start-only, so the restart step follows;
   *  a destination the consent grant does not cover pauses the line until
   *  Consent is reviewed. A refused write changes nothing in the form. */
  function routeToOaiy() {
    if (saving || loading) return;
    var others = settingsPatch(baseline, settings);
    for (var i = 0; i < ROUTE_KEYS.length; i++) delete others[ROUTE_KEYS[i]];
    write(
      {
        realtimeVoiceMode: 'desktop_realtime',
        realtimeVoiceEndpoint: OAIY.endpoint,
        realtimeVoiceDestination: OAIY.destination,
        aiReceptionist: true,
      },
      others
    );
  }

  /** Note the control in use (its caret too), to give focus back to once
   *  the form is drawn again. True when there was one. */
  function holdFocus() {
    var at = U.focusOf(root);
    if (at) heldFocus = at;
    return !!at;
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
  // text inputs never rebuild under a keystroke.
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

  /** OAIY Voice's state, live from the host's source list. */
  function oaiyVoiceText() {
    if (sources === undefined) return 'checking';
    if (sources === null) return 'unknown — the source list could not be read';
    var voice = OAIY.voiceService(sources);
    if (!voice) return 'not installed on this computer';
    return voice.status === 'running' ? 'running' : voice.status + ' — start it in OAIY Services';
  }

  /** One section of the form, drawn as a dashboard card. */
  function section(title, inner, extraClass) {
    return (
      '<section class="rcp-card' + (extraClass ? ' ' + extraClass : '') + '">' +
      '<div class="rcp-card__heading"><div class="rcp-card__heading-copy"><small>' + title + '</small></div></div>' +
      inner +
      '</section>'
    );
  }

  /** The live value for a "set in OAIY" row, '' when it cannot be read. */
  function liveValue(label) {
    if (label === 'Model') return OAIY.modelText(oaiyStatus);
    if (label === 'Voice on calls') return OAIY.voiceText(oaiyStatus);
    if (label === 'Speech') {
      var voice = sources ? OAIY.voiceService(sources) : null;
      return voice ? 'OAIY Voice ' + voice.status : '';
    }
    return '';
  }

  /** Where calls go (from the SAVED settings) and what is set in OAIY. */
  function routeHtml() {
    var route = OAIY.callRoute(baseline);
    var html = [];
    if (route === 'oaiy') {
      var voice = sources ? OAIY.voiceService(sources) : null;
      var voiceDown = !!voice && voice.status !== 'running';
      html.push(
        '<p class="rcp-notice' + (voiceDown ? ' rcp-notice--warn' : ' rcp-notice--ok') + '" role="status">' +
          '<strong>Calls go to OAIY.</strong> Aokie streams each call to OAIY Desktop on this computer. ' +
          'OAIY Voice hears the caller and speaks the replies; the Front desk agent writes them.' +
          (voiceDown ? ' OAIY Voice is ' + esc(voice.status) + ': start it in ' + U.navLink('services', 'Services') + '.' : '') +
          '</p>'
      );
    } else {
      // Off OAIY's route the receptionist would be Aokie's own, and flows and
      // you would speak in its voice: it has none. Another realtime provider
      // answers ordinary callers, but the lines Aokie says itself need a voice.
      var where =
        route === 'realtime'
          ? 'They are answered by another realtime provider (' + esc(OAIY.realtimeProviderId(baseline.realtimeVoiceEndpoint) || 'unknown') + '). ' +
            'Aokie has no voice of its own there, so its own lines (a screened caller’s message, the hold announcements) are not said and manager numbers ring through.'
          : esc(OAIY.noOwnSpeech) + ', so nobody answers them: they ring through to you.';
      html.push(
        '<div class="rcp-notice rcp-notice--warn" role="status"><strong>Calls do not go to OAIY.</strong> ' +
          where + ' Sending them to OAIY lets the Front desk agent answer in the OAIY voice; ' +
          'it applies after the receptionist restarts.' +
          '<div class="rcp-actions">' +
          '<button type="button" class="rcp-button is-primary" data-act="set-route-oaiy"' +
          (saving || loading ? ' disabled' : '') + '>Send calls to OAIY</button></div></div>'
      );
    }
    html.push(
      '<div class="rcp-settings-list rcp-set-in-oaiy">' +
        SET_IN_OAIY.map(function (row) {
          var live = liveValue(row[0]);
          var where = U.navLink(row[1], row[2]) + esc(row[3]);
          return (
            '<div class="rcp-setting"><small>' + esc(row[0]) + '</small><span>' +
            (live ? '<strong>' + esc(live) + '</strong> <em>in ' + where + '</em>' : where) +
            '</span></div>'
          );
        }).join('') +
        '</div>' +
        hint('These are set in OAIY Desktop, on the pages linked here. Aokie has no speech engines, voices or model of its own: all of these are OAIY’s.')
    );
    return html.join('');
  }

  /** Answering: on the OAIY route the receptionist toggle is locked on (the
   *  plugin refuses Desktop realtime without it), and OAIY Voice does the
   *  turn-taking, so Aokie's own pause and interruption settings step aside. */
  function answeringHtml(route) {
    var viaRealtime = route === 'oaiy' || route === 'realtime';
    // Off the realtime routes the receptionist would be Aokie's own, which
    // has no speech: nobody answers there, so there is no switch for it.
    var receptionist = viaRealtime
      ? '<label class="rcp-check"><input type="checkbox" checked disabled /><span>The AI receptionist answers calls</span></label>' +
        hint(
          route === 'oaiy'
            ? 'Always on while calls go to OAIY. To stop the Front desk answering, turn off Answer phone calls in ' + U.navLink('agent', 'Agent') + ' › Phone.'
            : 'Always on while calls go to a realtime provider.'
        )
      : hint(esc(OAIY.noOwnSpeech) + ', so nobody answers calls here. Send calls to OAIY, above, and the Front desk answers them.', true);
    return section(
      'Answering',
      receptionist +
        check('autoAnswer', 'Auto-answer incoming calls') +
        hint(
          route === 'oaiy'
            ? 'The phone rings until OAIY is ready to answer. If OAIY cannot be reached, it keeps ringing through to you.'
            : viaRealtime
              ? 'Aokie picks up once the provider is ready.'
              : 'Applies once calls go to OAIY: until then they ring through to you.'
        ) +
        check('agentHangup', 'The receptionist can end the call') +
        hint('Lets the receptionist hang up once the caller is done and the goodbye is said. When off, it leaves the line open for the caller to hang up. Applies after the receptionist restarts.') +
        (route === 'oaiy'
          ? hint('OAIY Voice decides when the caller has finished and stops a reply when the caller talks over it.')
          : '')
    );
  }

  /** The allowed list: who the receptionist deals with while trying it
   *  out, and what happens to everyone else (and to hidden numbers). */
  function allowedHtml() {
    var others = [
      ['ring', 'Leave them ringing on your phone'],
      ['reject', 'Reject them'],
    ];
    var empty = emptyListText();
    return section(
      'Who is answered',
      field(
        'Allowed numbers',
        // (the placeholder is plainly an example, in the reserved fictional range: not a number on the list)
        '<textarea rows="4" data-key="allowedNumbers" maxlength="' + TEXT_LIMITS.allowedNumbers + '"' +
          ' placeholder="For example:&#10;0491 570 156&#10;+61 491 570 157">' +
          esc(settings.allowedNumbers) + '</textarea>'
      ) +
        counterHtml('allowedNumbers') +
        hint('One number a line (or with commas), written in full with its area code: a number matches however it is written, by its last nine digits.') +
        '<p class="rcp-hint is-warn" id="set-allowed-short" aria-live="polite"' +
        (shortAllowedNumbers(settings.allowedNumbers).length ? '' : ' hidden') + '>' +
        esc(shortAllowedText(settings.allowedNumbers)) + '</p>' +
        check('callsAllowedOnly', 'Answer calls only from these numbers') +
        '<p class="rcp-hint is-warn" id="set-allowed-empty" role="status"' + (empty ? '' : ' hidden') + '>' +
        U.icons.alert + ' <span>' + esc(empty) + '</span></p>' +
        field(
          'Calls from other numbers',
          '<select data-key="otherCallers"' + (settings.callsAllowedOnly ? '' : ' disabled') + '>' +
            others.map(function (o) {
              return '<option value="' + o[0] + '"' + (settings.otherCallers === o[0] ? ' selected' : '') + '>' + esc(o[1]) + '</option>';
            }).join('') +
            '</select>'
        ) +
        hint(
          'Left ringing, the call shows on the Overview with Answer (the receptionist takes it) and Reject; answer it on the phone to take it yourself, and the receptionist stays out of it. ' +
            'While this is on, the receptionist never rings a number that is not on the list (a call back of a missed call, say).'
        ) +
        check('textsAllowedOnly', 'Text only these numbers') +
        hint('No text goes to any other number: not a reply, a follow-up or a missed-call text back. Texts from other numbers still arrive and are kept.') +
        check('rejectPrivate', 'Do not answer hidden or private numbers') +
        hint('Rejected while the phone rings when it says the number is hidden; a phone that only shows the number once a call is answered has it hung up straight away instead.')
    );
  }

  /** The allowed list's entries, split as the plugin splits it. */
  function allowedEntries(list) {
    return String(list || '')
      .split(/[\n,;]+/)
      .map(function (entry) {
        return entry.trim();
      })
      .filter(Boolean);
  }

  /** With "Answer calls only from these numbers" on and no number on the
   *  list, what that does ('' otherwise). */
  function emptyListText() {
    if (!settings.callsAllowedOnly || allowedEntries(settings.allowedNumbers).length) return '';
    return (
      (settings.otherCallers === 'reject'
        ? 'The list is empty, so every caller is rejected.'
        : 'The list is empty, so the receptionist answers nobody: every call rings through to you.') +
      ' Add a number, or turn off “Answer calls only from these numbers”.'
    );
  }

  /** The allowed-list entries with fewer than nine digits. The plugin
   *  compares numbers by their last nine digits (screen.rs): an entry of
   *  under six matches nobody, and one of six to eight only a caller id of
   *  exactly that many digits, which a number without its area code seldom
   *  is. Split as the plugin splits the list. */
  function shortAllowedNumbers(list) {
    return String(list || '')
      .split(/[\n,;]+/)
      .map(function (entry) {
        return entry.trim();
      })
      .filter(function (entry) {
        return entry && entry.replace(/\D/g, '').length < 9;
      });
  }

  function shortAllowedText(list) {
    var short = shortAllowedNumbers(list);
    if (!short.length) return '';
    var none = short.filter(function (entry) {
      return entry.replace(/\D/g, '').length < 6;
    });
    return (
      (none.length ? 'These match nobody: ' + none.join(', ') + '. ' : '') +
      (short.length > none.length
        ? 'These may miss the caller: ' +
          short
            .filter(function (entry) {
              return none.indexOf(entry) === -1;
            })
            .join(', ') +
          '. '
        : '') +
      'Write each number in full, with its area code.'
    );
  }

  /** What the receptionist says when no greeting is set: the plugin's own (radio/prompts.rs DEFAULT_GREETING, which
   *  scripts/check-receptionist-ui.mjs holds this to). A blank greeting means this one, never silence. */
  var DEFAULT_GREETING = 'Hello, thanks for calling. How can I help you today?';

  /** The first word of a name, without the punctuation around it. */
  function firstName(name) {
    return (String(name).trim().split(/\s+/)[0] || '').replace(/^[^\p{L}\p{N}]+|[^\p{L}\p{N}]+$/gu, '');
  }

  /** `text` without a leading `word` (in any case) when it starts with it as a whole word: [the word as written, the rest]. */
  function stripWord(text, word) {
    var head = text.slice(0, word.length);
    var rest = text.slice(word.length);
    return head.toLowerCase() === word.toLowerCase() && !/[\p{L}\p{N}]/u.test(rest.charAt(0)) ? [head, rest] : null;
  }

  /**
   * The greeting said to a caller OAIY knows (a name in its Contacts, or the one their phone shows), by first name: kept
   * as it is when it has the name, the name put after a "Hi" or "Hello" it starts with, or else "Hi Sam! " before it.
   * OAIY Desktop's own rule (voice::callers::personal_greeting, which greets calls that go to OAIY), said here as it is.
   */
  function personalGreeting(greeting, name) {
    var first = firstName(name);
    var hasName = String(greeting).split(/[^\p{L}\p{N}]+/u).some(function (w) { return w.toLowerCase() === first.toLowerCase(); });
    if (!first || !String(greeting).trim() || hasName) return greeting;
    var text = String(greeting).replace(/^\s+/, '');
    var hellos = ['hi', 'hello', 'hey', "g'day", 'g\u2019day'];
    for (var i = 0; i < hellos.length; i++) {
      var found = stripWord(text, hellos[i]);
      if (found) {
        // "Hi there!" is "Hi Sam!", not "Hi Sam there!".
        var there = found[1].charAt(0) === ' ' ? stripWord(found[1].slice(1), 'there') : null;
        return found[0] + ' ' + first + (there ? there[1] : found[1]);
      }
    }
    return 'Hi ' + first + '! ' + text;
  }

  /** What callers hear first, as the greeting box reads now: a new caller, and (calls that go to OAIY) one it knows. */
  function greetingSaidHtml(route) {
    var said = String(settings.greeting || '').trim() || DEFAULT_GREETING;
    return (
      '<span class="rcp-said-label">New callers hear</span><span>&ldquo;' + esc(said) + '&rdquo;</span>' +
      (route === 'oaiy'
        ? '<span class="rcp-said-label">Callers OAIY knows hear</span><span>&ldquo;' + esc(personalGreeting(said, 'Sam')) + '&rdquo;</span>' +
          '<span class="rcp-said-note">By first name: the one in OAIY&rsquo;s Contacts, or the one their phone shows.</span>'
        : '')
    );
  }

  /** The route the form was drawn for, for the greeting's lines as it is typed. */
  var greetingRoute = null;

  /** The plugin's own caps on the text fields (it refuses longer ones). */
  var TEXT_LIMITS = { greeting: 1000, persona: 4000, allowedNumbers: 4000 };

  /** "3,812 of 4,000 characters", shown from nine tenths of the cap. */
  function counterText(key) {
    var max = TEXT_LIMITS[key];
    var n = String(settings[key] || '').length;
    if (!max || n < max * 0.9) return '';
    return n.toLocaleString() + ' of ' + max.toLocaleString() + ' characters' + (n >= max ? ': the most it can hold' : '');
  }

  function counterHtml(key) {
    var text = counterText(key);
    return (
      '<p class="rcp-hint rcp-count" id="set-count-' + key + '" aria-live="polite"' + (text ? '' : ' hidden') + '>' +
      esc(text) + '</p>'
    );
  }

  /** The greeting and the brief Aokie sends with each call. */
  function greetingHtml(route) {
    greetingRoute = route;
    return section(
      'Greeting &amp; brief',
      field(
        'Greeting (spoken first)',
        '<input type="text" data-key="greeting" maxlength="' + TEXT_LIMITS.greeting + '" placeholder="' + esc(DEFAULT_GREETING) + '" value="' +
          esc(settings.greeting) + '" />'
      ) +
        counterHtml('greeting') +
        hint(
          route === 'oaiy'
            ? 'Spoken first, as soon as the call connects, in the voice chosen in OAIY. Blank = the built-in greeting shown in the box.'
            : 'Spoken first, as soon as the call connects. Blank = the built-in greeting shown in the box.'
        ) +
        '<p class="rcp-greeting-said" id="set-greeting-said" aria-live="polite">' + greetingSaidHtml(route) + '</p>' +
        field(
          'Receptionist brief',
          '<textarea rows="6" data-key="persona" maxlength="' + TEXT_LIMITS.persona + '" placeholder="e.g. We are a small hair salon. Cuts take 45 minutes.">' +
            esc(settings.persona) + '</textarea>'
        ) +
        counterHtml('persona') +
        hint(
          route === 'oaiy'
            ? 'Business notes sent with every call, inside Aokie’s fixed call rules. OAIY gives them to the Front desk agent as the receptionist brief; the Front desk’s own /brief.md and call instructions take precedence. Blank = short built-in notes: a small business taking questions, booking requests and messages.'
            : 'Business notes for the receptionist on every call. Blank = short built-in notes: a small business taking questions, booking requests and messages.'
        )
    );
  }

  /** Hardware: the Bluetooth link. */
  function advancedHtml(viaRealtime) {
    var codecOpts = [];
    for (var c = 0; c < AOKIE_CODEC_OPTIONS.length; c++) {
      var co = AOKIE_CODEC_OPTIONS[c];
      codecOpts.push(
        '<option value="' + co.value + '"' + (settings.hfpCodec === co.value ? ' selected' : '') + '>' +
          esc(co.label) +
          '</option>'
      );
    }
    return section(
      'Advanced',
      // Phone connection: the Aokie USB dongle is the only transport offered.
      // The native Windows-Bluetooth backend (transportMode setting) exists
      // for advanced use, but Windows 11 25H2 removed the OS hands-free
      // service — native mode cannot carry call audio there, so the mode is
      // pinned to the dongle in the UI rather than offered as a choice that
      // silently breaks calls.
      hint('Phone connection: Aokie USB dongle.') +
        // Dongle-only hardware knobs — meaningless on the native Windows
        // Bluetooth transport, so they hide (in place, order preserved)
        // unless the mode is dongle.
        '<div class="set-dongle-only"' + (settings.transportMode !== 'native' ? '' : ' hidden') + '>' +
        field('Bluetooth audio codec', '<select data-key="hfpCodec">' + codecOpts.join('') + '</select>') +
        hint('Dongle mode only. Some dongles only work reliably with CVSD; mSBC gives better speech-recognition accuracy where supported.') +
        // (a Windows device path, for a Windows quirk: on a Mac or Linux it does nothing, so it is not shown there)
        (ON_WINDOWS
          ? field(
              'Re-enumerate at start (hardware id)',
              '<input type="text" class="is-mono" data-key="reenumerateHwid" maxlength="200" placeholder="USB\\VID_0A5C&amp;PID_21EC" value="' +
                esc(settings.reenumerateHwid) + '" />'
            ) +
            hint('Dongle mode only. For a dongle whose call audio stays silent after Windows starts: Aokie re-enumerates this device before opening it, as if it were unplugged and plugged back in. Leave blank for none; takes effect the next time the phone line starts.')
          : '') +
        '</div>' +
        // The test tone is not played on realtime calls, so it only shows
        // where it does something.
        (viaRealtime
          ? ''
          : check('answerTone', 'Play a test tone on answer') +
            hint('Diagnostic: verifies the outbound audio path reaches the caller. Leave off for normal use.')) +
        '<div class="set-dongle-only"' + (settings.transportMode !== 'native' ? '' : ' hidden') + '>' +
        check('legacyPairingPin', 'Allow legacy PIN pairing (compatibility)') +
        hint(
          U.icons.alert + " Uses the fixed PIN 0000 for very old devices that can't do modern code-confirmation pairing — it provides no protection against a nearby impostor. Turn it on only while pairing such a device, then turn it back off.",
          true
        ) +
        '</div>'
    );
  }

  function formHtml() {
    var route = OAIY.callRoute(baseline);
    var viaRealtime = route === 'oaiy' || route === 'realtime';
    return (
      // A grid of cards that reflows with the width; "Where calls go" and the
      // actions span it.
      '<form class="rcp-form rcp-wide" id="set-form">' +
      section('Where calls go', routeHtml(), 'rcp-wide') +
      answeringHtml(route) +
      allowedHtml() +
      greetingHtml(route) +
      advancedHtml(viaRealtime) +
      // ---- Actions ---------------------------------------------------------
      // ⚠️ NOT type="submit": the sandboxed iframe (allow-scripts only, CSP
      // form-action 'none') BLOCKS native form submission BEFORE the submit
      // event fires — a submit button would be dead. Save rides the click
      // delegate; Enter-to-save rides the keydown handler in wire().
      '<div class="rcp-form__actions rcp-wide" id="set-bar">' +
      // Saved, but it applies once the receptionist restarts: said here, by
      // Save, where it is seen (at the top it was out of view on a phone).
      restartNoticeHtml() +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button is-primary" id="set-save" data-act="set-save"' + (saving || loading ? ' disabled' : '') + '>' +
      (saving ? 'Saving…' : 'Save') +
      '</button>' +
      '<button type="button" class="rcp-button" data-act="set-reload"' + (loading || saving || reloadArm ? ' disabled' : '') + '>Reload</button>' +
      '</div>' +
      dirtyHtml() +
      (reloadArm
        ? '<div class="rcp-confirm" role="group" aria-label="Reload over unsaved changes">' +
          '<p><strong>Throw away your unsaved changes?</strong> Reload puts back the settings as they are saved now.</p>' +
          '<div class="rcp-actions">' +
          '<button type="button" class="rcp-button is-danger" data-act="set-reload-confirm">Discard and reload</button>' +
          '<button type="button" class="rcp-button" data-act="set-reload-cancel">Keep editing</button>' +
          '</div>' +
          '</div>'
        : '') +
      // A failed save or reload, by the buttons that were pressed.
      (error ? '<p class="rcp-error" role="alert">' + esc(error) + '</p>' : '') +
      '</div>' +
      '</form>'
    );
  }

  function render() {
    if (!root) return;
    var body;
    if (loading && !loaded) {
      body = section('Receptionist settings', U.loadingHtml, 'rcp-wide');
    } else if (!loaded) {
      body = section(
        'Receptionist settings',
        (error === U.notRunning
          ? '<p class="rcp-empty">The settings show here once Aokie is running.</p>'
          : '<p class="rcp-error">The receptionist settings could not be read' + (error ? ': ' + esc(error) : '.') + '</p>') +
          '<div class="rcp-actions"><button type="button" class="rcp-button" data-act="set-retry">Try again</button></div>',
        'rcp-wide'
      );
    } else {
      body = formHtml();
    }
    // The form is drawn anew (a save, a read): the control in use, and its
    // caret, are given back, so what is typed next is not lost.
    // (one that cannot take focus yet, Save while saving, is held for the next drawing)
    var at = U.focusOf(root) || (U.focusLost(root) ? heldFocus : null);
    heldFocus = null;
    root.innerHTML = appManagedNoticeHtml() + body;
    if (at && !U.restoreFocus(root, at)) heldFocus = at;
    measureBar();
  }

  /** Saved, but where calls go applies only once the receptionist restarts:
   *  the step, and its button, in the Save bar, which stays in view. */
  function restartNoticeHtml() {
    if (!loaded || !pendingRestart) return '';
    return (
      '<div class="rcp-notice rcp-notice--warn rcp-bar-notice" role="status">' +
      '<span><strong>Saved. One more step:</strong> it applies when the receptionist restarts. ' +
      'The phone reconnects by itself, in about 15 seconds.</span>' +
      '<button type="button" class="rcp-button is-primary" data-act="set-apply-restart"' + (saving || loading || restarting ? ' disabled' : '') + '>' +
      (restarting ? 'Restarting…' : 'Restart receptionist now') +
      '</button>' +
      '</div>'
    );
  }

  // ---- the Save bar's height -------------------------------------------------
  // It is sticky at the foot of the window, so a field the keyboard moves to
  // must stop above it, not under it: the page keeps that much clear below
  // (scroll-padding and the fields' scroll-margin read --rcp-bar-h), and a
  // field focused under it is scrolled up anyway.

  var barObserver = null;

  function measureBar() {
    var bar = root && root.querySelector ? root.querySelector('#set-bar') : null;
    var docEl = document.documentElement;
    if (!docEl || !docEl.style) return;
    if (!bar) {
      forgetBar();
      return;
    }
    var set = function () {
      docEl.style.setProperty('--rcp-bar-h', Math.ceil(bar.getBoundingClientRect().height) + 'px');
    };
    set();
    if (barObserver) barObserver.disconnect();
    barObserver = typeof ResizeObserver === 'function' ? new ResizeObserver(set) : null;
    if (barObserver) barObserver.observe(bar);
  }

  function forgetBar() {
    if (barObserver) barObserver.disconnect();
    barObserver = null;
    var docEl = document.documentElement;
    if (docEl && docEl.style) docEl.style.removeProperty('--rcp-bar-h');
  }

  /** A field focused under the Save bar (where the browser left it) is
   *  scrolled up to sit just above it. */
  function keepAboveBar(e) {
    var t = e.target;
    var bar = root && root.querySelector ? root.querySelector('#set-bar') : null;
    if (!t || !bar || bar.contains(t) || !t.getBoundingClientRect) return;
    window.requestAnimationFrame(function () {
      var r = t.getBoundingClientRect();
      var top = bar.getBoundingClientRect().top;
      var tall = r.height > top * 0.6; // (a field taller than the room above the bar: its top in view is enough)
      var over = (tall ? r.top + 40 : r.bottom) - (top - 8);
      if (over > 0) window.scrollBy(0, over);
    });
  }

  /** Beside Save: whether the form holds edits not saved yet. */
  function dirtyHtml() {
    var dirty = hasUnsavedEdits();
    return (
      '<span class="rcp-dirty' + (dirty ? ' is-dirty' : '') + '" id="set-dirty" aria-live="polite">' +
      (dirty ? 'Unsaved changes' : 'No unsaved changes') +
      '</span>'
    );
  }

  function updateDirty() {
    var el = root && root.querySelector ? root.querySelector('#set-dirty') : null;
    if (!el) return;
    var dirty = hasUnsavedEdits();
    el.className = 'rcp-dirty' + (dirty ? ' is-dirty' : '');
    el.textContent = dirty ? 'Unsaved changes' : 'No unsaved changes';
  }

  /**
   * The honest answer to "I changed the greeting and it did not take".
   *
   * Something other than this form can write the greeting and the brief (a
   * linked app re-applying its record, a flow calling settings.set). Shown
   * once the saved greeting or brief has actually been seen to change under
   * the form, by a write it did not make: any other settings write (another
   * tab, OAIY's Agent › Phone page) bumps the version too and is no sign of
   * that. Said without naming who, since this form cannot know.
   */
  function appManagedNoticeHtml() {
    if (!loaded || !appManaged) return '';
    return (
      '<p class="rcp-notice rcp-notice--warn rcp-wide" role="status">' +
      '<strong>The greeting or the brief was just changed outside this form' +
      (typeof configVersion === 'number' ? ' (config v' + configVersion + ')' : '') +
      '.</strong> ' +
      'The form shows what is saved now. If something keeps writing them (a linked app or a flow that ' +
      'sets the receptionist&rsquo;s settings on each call), change them there to make an edit stick.' +
      '</p>'
    );
  }

  // ---- event wiring (delegated once per container) ------------------------

  function onInputOrChange(e) {
    handleField(e);
    updateDirty();
  }

  function handleField(e) {
    var t = e.target;
    if (!t || !t.getAttribute) return;

    var key = t.getAttribute('data-key');
    if (key != null) {
      settings[key] = t.value;
      // (the greeting's lines follow the box as it is typed)
      if (key === 'greeting' && root) {
        var said = root.querySelector('#set-greeting-said');
        if (said) said.innerHTML = greetingSaidHtml(greetingRoute);
      }
      // (the allowed list says which of its numbers are too short, as typed)
      if (key === 'allowedNumbers' && root) {
        var short = root.querySelector('#set-allowed-short');
        if (short) {
          short.textContent = shortAllowedText(t.value);
          short.hidden = !short.textContent;
        }
      }
      // (near a field's cap, how much of it is used)
      if (TEXT_LIMITS[key] && root && root.querySelector) {
        var count = root.querySelector('#set-count-' + key);
        if (count) {
          count.textContent = counterText(key);
          count.hidden = !count.textContent;
        }
      }
      if (key === 'allowedNumbers' || key === 'otherCallers') updateEmptyList();
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
    var boolKey = t.getAttribute('data-bool');
    if (boolKey != null && e.type === 'change') {
      settings[boolKey] = !!t.checked;
      // (what happens to other callers is asked only while the list is in force)
      if (boolKey === 'callsAllowedOnly' && root) {
        var others = root.querySelector('select[data-key="otherCallers"]');
        if (others) others.disabled = !t.checked;
        updateEmptyList();
      }
    }
  }

  /** The empty-list warning, as the list and its switch are changed. */
  function updateEmptyList() {
    var warn = root && root.querySelector ? root.querySelector('#set-allowed-empty') : null;
    if (!warn) return;
    var text = emptyListText();
    var words = warn.querySelector('span');
    if (words) words.textContent = text;
    warn.hidden = !text;
  }

  function wire(el) {
    if (el.__aokieSettingsWired) return;
    el.__aokieSettingsWired = true;
    el.addEventListener('input', onInputOrChange);
    el.addEventListener('change', onInputOrChange);
    el.addEventListener('focusout', onInputOrChange);
    el.addEventListener('focusin', keepAboveBar);
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
      else if (act === 'set-route-oaiy') routeToOaiy();
      else if (act === 'set-apply-restart') applyRestart();
      else if (act === 'set-reload' && hasUnsavedEdits()) {
        // Reload throws the edits away: asked first, in the bar.
        reloadArm = true;
        render();
        var keepBtn = root && root.querySelector ? root.querySelector('[data-act="set-reload-cancel"]') : null;
        if (keepBtn) keepBtn.focus();
      } else if (act === 'set-reload-cancel') {
        reloadArm = false;
        focusReloadAfter();
        render();
      } else if (act === 'set-reload' || act === 'set-reload-confirm' || act === 'set-retry') {
        reloadArm = false;
        if (act !== 'set-retry') focusReloadAfter();
        load(false);
      }
    });
  }

  /** The confirm's buttons go with it: Reload has focus once the form is
   *  drawn again (and enabled, once a read is over). */
  function focusReloadAfter() {
    var a = document.activeElement;
    if (a && root && root.contains(a) && typeof a.blur === 'function') a.blur();
    heldFocus = { sel: '[data-act="set-reload"]' };
  }

  /** Apply start-only settings by restarting THIS plugin through the host
   *  bridge. The screen keeps running; the plugin (and the phone link)
   *  reconnect underneath it. Older Desktops without the bridge verb get an
   *  honest pointer at the manual path. */
  function applyRestart() {
    if (restarting) return;
    if (typeof HOST.restartPlugin !== 'function') {
      HOST.toast(
        'info',
        'This version of OAIY cannot restart Aokie from here: restart the Aokie plugin on the Plugins page to apply.'
      );
      return;
    }
    // A restart drops the phone link, and with it a call on the line
    // (review 1 #31): ask first, and keep the restart for when it is over.
    // A plugin that cannot say whether a call is on (it did not answer) is
    // not restarted either: that could be the call it drops (review 3 #31).
    restarting = true;
    render();
    HOST.command('call.current').then(
      function (data) {
        var call = data && data.call;
        if (call && call.state !== 'ended') {
          restarting = false;
          render();
          HOST.toast(
            'info',
            'A call is on the line, so the receptionist was not restarted. Press Restart receptionist now again once the call has ended.'
          );
          return;
        }
        restartNow();
      },
      function (e) {
        restarting = false;
        render();
        HOST.toast(
          'error',
          'Aokie could not say whether a call is on the line (' + errMsg(e) + '), so it was not restarted. Try again in a moment.'
        );
      }
    );
  }

  function restartNow() {
    pendingRestart = null;
    restarting = true;
    render();
    HOST.toast('info', 'Restarting the receptionist: the phone reconnects by itself.');
    HOST.restartPlugin().then(
      function () {
        restarting = false;
        HOST.toast('success', 'Receptionist restarted: the saved settings are live.');
        // Give the plugin a beat to finish booting before re-reading
        // settings (an immediate settings.get can race the connector start).
        setTimeout(function () {
          load(true);
        }, 3000);
      },
      function (e) {
        restarting = false;
        HOST.toast('error', 'The receptionist could not be restarted: ' + errMsg(e));
        pendingRestart = ['restart'];
        render();
      }
    );
  }

  TABS.register('settings', {
    mount: function (el) {
      root = el;
      wire(el);
      if (loaded) {
        render();
        // A linked app and OAIY Voice can both change while another tab is
        // open. Refresh on every return; preserve a local draft if one
        // exists, and never turn this read path into an implicit save.
        load(true);
      } else {
        load(false);
      }
    },
    unmount: function () {
      root = null;
      reloadArm = false;
      heldFocus = null;
      forgetBar();
    },
  });
})();
