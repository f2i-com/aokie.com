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
 * brief it sends with each call, conversation handling and the hardware.
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
    // numbers) are set only there; they write Aokie's own screening and
    // outbound keys, so this form does not offer a second copy.
    ['Callbacks and screening', 'agent', 'Agent', ' › Phone: missed-call callbacks, who is answered, and blocked numbers'],
    ['Speech', 'services', 'Services', ': speech to text, and the call voice'],
    ['Model', 'engines', 'Engines', ', which the Front desk agent’s OAIY provider follows (Agent › Settings › AI providers)'],
    ['Voice on calls', 'calendar', 'Calendar', ' › Hours & services › Voice on calls'],
    ['Hours, services and booking', 'calendar', 'Calendar', ' › Hours & services; the receptionist looks these up during calls'],
  ];

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

  /** True once an external writer has been observed bumping configVersion. */
  function noteConfigVersion(next, ours) {
    if (typeof next !== 'number') return;
    if (configVersion !== null && next !== configVersion && !ours) {
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
        noteConfigVersion(data.configVersion, false);
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
      HOST.toast('success', 'No changes to save');
      return Promise.resolve(false);
    }
    saving = true;
    error = null;
    render();
    var ok = false;
    return HOST.command('settings.set', patch)
      .then(
        function (data) {
          ok = true;
          var merged = withAokieDefaults((data || {}).settings);
          // Separate objects — see the aliasing note in load().
          baseline = merged;
          settings = withAokieDefaults(merged);
          // The saved mode now drives the Dongle tab's visibility too.
          if (TABS.transport && TABS.transport.update) TABS.transport.update(merged);
          // OUR bump — never mistake a successful save for the linked app.
          noteConfigVersion((data || {}).configVersion, true);
          var blocked = data && typeof data.blocked === 'string' ? data.blocked.trim() : '';
          // Start-only keys (where calls go and friends) are saved but NOT
          // live until the plugin restarts — surface the apply step instead
          // of letting the change look silently ignored.
          var pend = (data || {}).appliesAtReconnect;
          pendingRestart = pend && pend.length ? pend.slice() : null;
          if (blocked) {
            HOST.toast(
              'error',
              'Settings saved, but the receptionist is paused. Open Consent and accept the new data destination before calls can resume.'
            );
          } else if (pendingRestart) {
            HOST.toast(
              'success',
              'Saved — one more step: press "Restart receptionist now" below to apply it to the line.'
            );
          } else {
            HOST.toast('success', 'Receptionist settings saved — takes effect on the next call.');
          }
        },
        function (e) {
          error = errMsg(e);
        }
      )
      .then(function () {
        saving = false;
        render();
        return ok;
      });
  }

  var ROUTE_KEYS = ['realtimeVoiceMode', 'realtimeVoiceEndpoint', 'realtimeVoiceDestination', 'aiReceptionist'];

  /** Point calls at OAIY's voice gateway and save. The route keys are
   *  start-only, so the restart banner follows; a destination the consent
   *  grant does not cover pauses the line until Consent is reviewed. A
   *  refused save puts the route keys back, so a later Save of something
   *  else never retries it unasked. */
  function routeToOaiy() {
    var before = {};
    for (var i = 0; i < ROUTE_KEYS.length; i++) before[ROUTE_KEYS[i]] = settings[ROUTE_KEYS[i]];
    settings.realtimeVoiceMode = 'desktop_realtime';
    settings.realtimeVoiceEndpoint = OAIY.endpoint;
    settings.realtimeVoiceDestination = OAIY.destination;
    settings.aiReceptionist = true;
    save().then(function (saved) {
      if (saved) return;
      for (var k in before) settings[k] = before[k];
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
      var where =
        route === 'realtime'
          ? 'another realtime provider (' + esc(OAIY.realtimeProviderId(baseline.realtimeVoiceEndpoint) || 'unknown') + ')'
          : route === 'local'
            ? 'Aokie’s own older speech and model on this computer'
            : 'no AI receptionist — your flows or you speak to callers';
      html.push(
        '<p class="rcp-notice rcp-notice--warn" role="status"><strong>Calls do not go to OAIY.</strong> ' +
          'They are answered by ' + where + '. Sending them to OAIY lets the Front desk agent answer in the OAIY voice; ' +
          'it applies after the receptionist restarts.</p>' +
          '<div class="rcp-actions">' +
          '<button type="button" class="rcp-button is-primary" data-act="set-route-oaiy"' +
          (saving || loading ? ' disabled' : '') + '>Send calls to OAIY</button></div>'
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
        hint('These are set in OAIY Desktop, on the pages linked here. Aokie’s older speech engines, voices and model settings are not used for calls that go to OAIY.')
    );
    return html.join('');
  }

  /** Answering: on the OAIY route the receptionist toggle is locked on (the
   *  plugin refuses Desktop realtime without it), and OAIY Voice does the
   *  turn-taking, so Aokie's own pause and interruption settings step aside. */
  function answeringHtml(route) {
    var toOaiy = route === 'oaiy';
    var receptionist = toOaiy
      ? '<label class="rcp-check"><input type="checkbox" checked disabled /><span>The AI receptionist answers calls</span></label>' +
        hint('Always on while calls go to OAIY. To stop the Front desk answering, turn off Answer phone calls in ' + U.navLink('agent', 'Agent') + ' › Phone.')
      : check('aiReceptionist', 'The AI receptionist answers calls') +
        hint('When off, Aokie only bridges the call: your flows or you (Speak, on the Overview) talk to the caller.');
    return section(
      'Answering',
      receptionist +
        check('autoAnswer', 'Auto-answer incoming calls') +
        hint(
          toOaiy
            ? 'The phone rings until OAIY is ready to answer. If OAIY cannot be reached, it keeps ringing through to you.'
            : 'Aokie picks up as soon as a call rings.'
        ) +
        check('agentHangup', 'The receptionist can end the call') +
        hint('Lets the receptionist hang up once the caller is done and the goodbye is said. When off, it leaves the line open for the caller to hang up. Applies after the receptionist restarts.') +
        (toOaiy
          ? hint('OAIY Voice decides when the caller has finished and stops a reply when the caller talks over it.')
          : '')
    );
  }

  /** The greeting and the brief Aokie sends with each call. */
  function greetingHtml(route) {
    return section(
      'Greeting &amp; brief',
      field(
        'Greeting (spoken first)',
        '<input type="text" data-key="greeting" placeholder="Thanks for calling! How can I help you today?" value="' +
          esc(settings.greeting) + '" />'
      ) +
        hint(
          route === 'oaiy'
            ? 'Spoken first, as soon as the call connects, in the voice chosen in OAIY. Blank = a friendly built-in default.'
            : 'Spoken first, as soon as the call connects. Blank = a friendly built-in default.'
        ) +
        field(
          'Receptionist brief',
          '<textarea rows="6" data-key="persona" placeholder="e.g. We are a small hair salon. Cuts take 45 minutes.">' +
            esc(settings.persona) + '</textarea>'
        ) +
        hint(
          route === 'oaiy'
            ? 'Business notes sent with every call, inside Aokie’s fixed call rules. OAIY gives them to the Front desk agent as the receptionist brief; the Front desk’s own /brief.md and call instructions take precedence. Blank = no notes, only the call rules.'
            : 'Business notes for the receptionist on every call. Blank = the built-in receptionist script.'
        )
    );
  }

  /** Aokie's own turn-taking — used only when Aokie's older speech answers. */
  function tuningHtml() {
    return section(
      'Conversation tuning',
      hint('These apply to calls Aokie’s own speech answers, not to calls that go to OAIY.') +
        field(
          'Wait after the caller pauses (ms)',
          '<input type="number" data-num="sttEndpointMs" min="150" max="2000" step="50" value="' +
            esc(settings.sttEndpointMs) + '" />'
        ) +
        hint(
          'How long the caller must pause before Aokie treats their turn as finished. Lower = snappier, but risks cutting off mid-sentence pauses.'
        ) +
        check('bargeIn', 'Listen while speaking and allow interruptions') +
        hint('Keep the caller’s words while Aokie talks. Sustained speech or “stop” makes Aokie yield; brief interjections are kept for the next turn. Requires live call transcription. Applies after reconnecting.') +
        check('conversationAcknowledgements', 'Let Aokie briefly acknowledge longer explanations') +
        hint('A short “Mm-hm” at a brief pause lets the caller know Aokie is listening. Their turn stays open, and they can interrupt the acknowledgement. Requires listening while speaking; applies after reconnecting.') +
        field(
          'Interruption threshold',
          '<input type="number" data-num="bargeSensitivity" min="100" max="2000" step="25" value="' +
            esc(settings.bargeSensitivity) + '" />'
        ) +
        hint('Lower = easier to interrupt; too low may react to background noise. Start around 550–650. With listening off, caller recognition is muted during replies.')
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
        '<div class="set-dongle-only"' + (settings.transportMode === 'dongle' ? '' : ' hidden') + '>' +
        field('Bluetooth audio codec', '<select data-key="hfpCodec">' + codecOpts.join('') + '</select>') +
        hint('Dongle mode only. Some dongles only work reliably with CVSD; mSBC gives better speech-recognition accuracy where supported.') +
        field(
          'Re-enumerate at start (hardware id)',
          '<input type="text" data-key="reenumerateHwid" maxlength="200" placeholder="USB\\VID_0A5C&amp;PID_21EC" value="' +
            esc(settings.reenumerateHwid) + '" />'
        ) +
        hint('Dongle mode only. For a dongle whose call audio stays silent after Windows starts: Aokie re-enumerates this device before opening it, as if it were unplugged and plugged back in. Leave blank for none; takes effect the next time the phone line starts.') +
        '</div>' +
        // The test tone is not played on realtime calls, so it only shows
        // where it does something.
        (viaRealtime
          ? ''
          : check('answerTone', 'Play a test tone on answer') +
            hint('Diagnostic: verifies the outbound audio path reaches the caller. Leave off for normal use.')) +
        '<div class="set-dongle-only"' + (settings.transportMode === 'dongle' ? '' : ' hidden') + '>' +
        check('legacyPairingPin', 'Allow legacy PIN pairing (compatibility)') +
        hint(
          "⚠️ Uses the fixed PIN 0000 for very old devices that can't do modern code-confirmation pairing — it provides no protection against a nearby impostor. Enable only while pairing such a device, then turn it back off.",
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
      greetingHtml(route) +
      (viaRealtime ? '' : tuningHtml()) +
      advancedHtml(viaRealtime) +
      // ---- Actions ---------------------------------------------------------
      // ⚠️ NOT type="submit": the sandboxed iframe (allow-scripts only, CSP
      // form-action 'none') BLOCKS native form submission BEFORE the submit
      // event fires — a submit button would be dead. Save rides the click
      // delegate; Enter-to-save rides the keydown handler in wire().
      '<div class="rcp-form__actions rcp-wide">' +
      (pendingRestart
        ? '<p class="rcp-notice rcp-notice--warn">Saved — the change applies when the receptionist restarts. The phone reconnects automatically (about 15 seconds of downtime).</p>' +
          '<div class="rcp-actions">' +
          '<button type="button" class="rcp-button is-primary" data-act="set-apply-restart"' + (saving || loading ? ' disabled' : '') + '>Restart receptionist now</button>' +
          '</div>'
        : '') +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button is-primary" id="set-save" data-act="set-save"' + (saving || loading ? ' disabled' : '') + '>' +
      (saving ? 'Saving…' : 'Save') +
      '</button>' +
      '<button type="button" class="rcp-button" data-act="set-reload"' + (loading || saving ? ' disabled' : '') + '>Reload</button>' +
      '</div>' +
      '</div>' +
      '</form>'
    );
  }

  function render() {
    if (!root) return;
    var body;
    if (loading && !loaded) {
      body = section('Receptionist settings', '<p class="rcp-loading">Loading…</p>', 'rcp-wide');
    } else if (!loaded) {
      body = section(
        'Receptionist settings',
        '<p class="rcp-inline-note">Couldn\'t load receptionist settings.' +
          (error ? ' ' + esc(error) : '') +
          '</p><div class="rcp-actions"><button type="button" class="rcp-button" data-act="set-retry">Retry</button></div>',
        'rcp-wide'
      );
    } else {
      body = formHtml();
    }
    root.innerHTML =
      appManagedNoticeHtml() +
      body +
      (loaded && error ? '<p class="rcp-error rcp-wide">' + esc(error) + '</p>' : '');
  }

  /**
   * The honest answer to "I changed the greeting and it did not take".
   *
   * A linked FormLogic app re-applies its Receptionist Settings record on
   * every incoming call, so a greeting or brief edited here is saved and then
   * replaced. Shown ONCE an external configVersion bump has actually been
   * observed, so a receptionist with no linked app, where this form IS the
   * source of truth, never sees it.
   */
  function appManagedNoticeHtml() {
    if (!loaded || !appManaged) return '';
    return (
      '<p class="rcp-notice rcp-notice--warn rcp-wide" role="status">' +
      '<strong>Your FormLogic app just re-applied these settings' +
      (typeof configVersion === 'number' ? ' (config v' + configVersion + ')' : '') +
      '.</strong> ' +
      'It does that on every incoming call, so a greeting or brief edited here is replaced by the ' +
      'app&rsquo;s Receptionist Settings record. Edit them in the app to make them stick.' +
      '</p>'
    );
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
    var boolKey = t.getAttribute('data-bool');
    if (boolKey != null && e.type === 'change') {
      settings[boolKey] = !!t.checked;
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
      else if (act === 'set-route-oaiy') routeToOaiy();
      else if (act === 'set-apply-restart') applyRestart();
      else if (act === 'set-reload' || act === 'set-retry') load(false);
    });
  }

  /** Apply start-only settings by restarting THIS plugin through the host
   *  bridge. The screen keeps running; the plugin (and the phone link)
   *  reconnect underneath it. Older Desktops without the bridge verb get an
   *  honest pointer at the manual path. */
  function applyRestart() {
    if (typeof HOST.restartPlugin !== 'function') {
      HOST.toast(
        'info',
        'This OAIY Desktop cannot restart plugins from here yet — restart the Aokie plugin from the Plugins panel to apply.'
      );
      return;
    }
    pendingRestart = null;
    render();
    HOST.toast('info', 'Restarting the receptionist — the phone reconnects automatically.');
    HOST.restartPlugin().then(
      function () {
        HOST.toast('success', 'Receptionist restarted — the saved settings are live.');
        // Give the plugin a beat to finish booting before re-reading
        // settings (an immediate settings.get can race the connector start).
        setTimeout(function () {
          load(true);
        }, 3000);
      },
      function (e) {
        HOST.toast('error', 'Restart failed: ' + errMsg(e));
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
    },
  });
})();
