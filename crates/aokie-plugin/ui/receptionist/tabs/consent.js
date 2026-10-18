/*
 * Consent tab — CONSENT-001, the consent status + wizard FormLogic Desktop
 * owns, ported faithfully from desktop/src/aokie/ConsentWizard.tsx
 * (ConsentSection + ConsentWizard; SCOPE_ROWS copy verbatim).
 *
 * Before the Aokie receptionist touches a phone line, the OPERATOR must
 * accept a versioned, scoped grant covering exactly what the system will
 * access and where that data can be sent. Accepting issues a grant SIGNED by
 * this Desktop's per-install key (PluginHost.consent.issue) and flips
 * enforcement on (`settings.set consentMode='enforce'`), so a denied scope
 * is refused at the point of capture, not just hidden in the UI.
 *
 * Sandbox deltas: the wizard renders INLINE (no modal overlay in the tab),
 * and the privacy-disclosure link renders as plain text (the sandboxed
 * iframe cannot open external pages). The compiled surface offers no revoke
 * button — re-consent via the wizard is the only path, ported as-is.
 */
(function () {
  'use strict';

  var HOST = window.PluginHost;
  var TABS = window.__aokieTabs;
  if (!HOST || !TABS) return;
  var U = TABS.util;
  var esc = U.esc;
  var errMsg = U.errMsg;
  var ICONS = U.icons;
  var OAIY = TABS.oaiy;
  /** Inside OAIY Desktop (not FormLogic Desktop): its words, and its own
   *  voice gateway as where calls go. */
  var IN_OAIY = !!(OAIY && OAIY.isHost);

  // The Desktop is a loopback broker, but these two reserved provider routes
  // delegate transcript text to OpenAI. They therefore share one stable,
  // account-free effective destination in the signed consent grant. Every
  // other loopback route remains local.
  var CODEX_DESTINATION = 'OpenAI ChatGPT via Codex';
  var CODEX_ID_NONE = 'openai-codex-agent-none';
  var CODEX_ID_LOW = 'openai-codex-agent-low';

  function isLoopbackUrl(raw) {
    try {
      var parsed = new URL(raw);
      var host = parsed.hostname.toLowerCase();
      var ipHost = host[0] === '[' && host[host.length - 1] === ']' ? host.slice(1, -1) : host;
      return (
        host === 'localhost' ||
        ipHost === '::1' ||
        ipHost === '::' ||
        /^::ffff:7f[0-9a-f]{2}:[0-9a-f]{1,4}$/.test(ipHost) ||
        ipHost === '::ffff:0:0' ||
        host === '0.0.0.0' ||
        /^127(?:\.[0-9]{1,3}){3}$/.test(host)
      );
    } catch (e) {
      return false;
    }
  }

  function isCodexLiveCallEndpoint(raw) {
    try {
      var parsed = new URL(String(raw || '').trim());
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
        return false;
      }
      var providerId = decodeURIComponent(segments[4]);
      return (
        isLoopbackUrl(parsed.href) &&
        (parsed.protocol === 'http:' || parsed.protocol === 'https:') &&
        parsed.port === '17872' &&
        (providerId === CODEX_ID_NONE || providerId === CODEX_ID_LOW)
      );
    } catch (e) {
      return false;
    }
  }

  function isDesktopRealtimeEndpoint(raw) {
    try {
      var parsed = new URL(String(raw || '').trim());
      var segments = parsed.pathname.split('/');
      return (
        parsed.protocol === 'ws:' &&
        parsed.hostname === '127.0.0.1' &&
        parsed.port === '17872' &&
        parsed.username === '' &&
        parsed.password === '' &&
        !parsed.search &&
        !parsed.hash &&
        segments.length === 8 &&
        segments[1] === 'api' &&
        segments[2] === 'ai' &&
        segments[3] === 'providers' &&
        /^[A-Za-z0-9._-]{1,128}$/.test(decodeURIComponent(segments[4])) &&
        segments[5] === 'v1' &&
        segments[6] === 'realtime' &&
        segments[7] === 'stream'
      );
    } catch (e) {
      return false;
    }
  }

  function effectiveDestination(raw) {
    var value = String(raw || '').trim();
    if (!value) return '';
    if (isCodexLiveCallEndpoint(value)) return CODEX_DESTINATION;
    if (value === CODEX_DESTINATION) return CODEX_DESTINATION;
    if (isDesktopRealtimeEndpoint(value)) return '';
    if (isLoopbackUrl(value)) return '';
    try {
      return new URL(value).origin;
    } catch (e) {
      return value;
    }
  }

  // ---- SCOPE_ROWS — copy VERBATIM from ConsentWizard.tsx ------------------

  var SCOPE_ROWS = [
    {
      key: 'bluetooth',
      label: 'Bluetooth phone link',
      detail:
        'Pair with your phone and handle calls through the Aokie dongle. Required for every phone feature — deny it and the receptionist stays offline.',
      defaultOn: true,
    },
    {
      key: 'transcription',
      label: 'Live call transcription',
      detail:
        "Caller audio is transcribed so the receptionist can understand and reply. Deny it and NO audio reaches any speech engine — calls can still ring, but the AI can't converse.",
      defaultOn: true,
    },
    {
      key: 'sms',
      label: 'SMS (read + send)',
      detail:
        'Read message threads and send replies/confirmations through your phone (MAP profile).',
      defaultOn: true,
    },
    {
      key: 'contacts',
      label: 'Contacts (caller names)',
      detail: 'Look up the caller in your phone book (PBAP) so records show a name, not a number.',
      defaultOn: true,
    },
    {
      key: 'recording',
      label: 'Call audio recording',
      detail:
        'Store raw call audio. Not currently used by any feature — leave off unless a future feature asks for it.',
      defaultOn: false,
    },
    {
      key: 'remoteCaptions',
      label: 'Companion live captions',
      detail:
        'Send permission-filtered live caption text to authorised Companion users through the selected FormLogic or custom signalling server.',
      // (inside OAIY there is no FormLogic server: the one the apps connect through)
      oaiyDetail:
        'Send permission-filtered live caption text to authorised Companion users through the signalling server your Companion apps connect through.',
      defaultOn: false,
    },
    {
      key: 'remoteAssistance',
      label: 'Companion private assistance',
      detail:
        'Let Aokie ask an authorised team member one bounded question and accept one typed answer. Full transcripts and arbitrary recipient selection are not included.',
      defaultOn: false,
    },
    {
      key: 'remoteMonitoring',
      label: 'Companion listen-only audio',
      detail:
        'Bridge caller audio over endpoint-encrypted WebRTC to authorised observers. Their microphones are not requested and cannot publish to the caller path.',
      defaultOn: false,
    },
    {
      key: 'remoteConsult',
      label: 'Companion private voice consultation',
      detail:
        'Place the caller on software hold and open an isolated Aokie-to-operator voice lane. Caller audio cannot enter the consult and consult audio cannot reach the caller.',
      defaultOn: false,
    },
    {
      key: 'remoteTakeover',
      label: 'Companion call takeover',
      detail:
        "Let one authorised Companion user speak to the caller through Aokie's encrypted media bridge using the Companion device's microphone and speakers. Desktop and the Aokie plugin relay the audio; they do not pair local audio hardware. This is not a carrier transfer.",
      defaultOn: false,
    },
  ];

  // ---- state --------------------------------------------------------------

  var root = null;
  var status = undefined; // consent.get result: undefined loading, null failed
  var settingsBag = {}; // live settings (endpoints become the destination list)
  var scopes = {};
  var scopesSeeded = false;
  // How long call records are kept: the grant's own when there is one (a
  // 30-day grant reopened at 90), as typed (retentionText), 1 to 3650 days.
  var RETENTION_MAX = 3650;
  var retentionDays = 90;
  var retentionText = '90';
  var retentionSeeded = false;
  var wizardOpen = false;
  var submitting = false;
  var error = null; // the consent form's own: an Accept that failed or was held back
  var loadError = null; // consent.get's: the next good read clears it
  var loadingNow = false;
  var pollTimer = null;
  var codexProviderAvailable = false;
  var codexDestinationOptIn = false;
  var destinationSeeded = false;
  var setupFormOpened = false; // setup mode has opened the form once

  for (var i = 0; i < SCOPE_ROWS.length; i++) scopes[SCOPE_ROWS[i].key] = SCOPE_ROWS[i].defaultOn;

  function load() {
    loadingNow = true;
    var consentP = HOST.consent.get().then(
      function (s) {
        status = s || null;
        // A previous read's failure must not haunt a now-healthy status
        // readout. (The form's own error is not the read's: accept() keeps
        // it, and the poll below must not clear it before it is read.)
        loadError = null;
        // Re-consent starts from what was previously granted.
        if (!scopesSeeded && s && s.grant && s.grant.scopes) {
          for (var i = 0; i < SCOPE_ROWS.length; i++) {
            var key = SCOPE_ROWS[i].key;
            if (typeof s.grant.scopes[key] === 'boolean') scopes[key] = s.grant.scopes[key];
          }
          scopesSeeded = true;
        }
        if (!retentionSeeded && s && s.grant && s.grant.scopes) {
          var kept = s.grant.scopes.retentionDays;
          if (typeof kept === 'number' && kept === Math.floor(kept) && kept >= 1 && kept <= RETENTION_MAX) {
            retentionDays = kept;
            retentionText = String(kept);
          }
          retentionSeeded = true;
        }
        if (!destinationSeeded && s && s.grant && s.grant.scopes) {
          var grantedDestinations = Array.isArray(s.grant.scopes.destinations)
            ? s.grant.scopes.destinations
            : [];
          for (var d = 0; d < grantedDestinations.length; d++) {
            if (effectiveDestination(grantedDestinations[d]) === CODEX_DESTINATION) {
              codexDestinationOptIn = true;
              break;
            }
          }
          destinationSeeded = true;
        }
      },
      function (e) {
        if (status === undefined) status = null;
        loadError = errMsg(e);
      }
    );
    // The settings bag feeds the "where call data goes" destination list —
    // best-effort (the wizard degrades to the local-processing paragraph).
    var settingsP = HOST.command('settings.get').then(
      function (data) {
        settingsBag = (data && data.settings) || {};
      },
      function () {
        settingsBag = {};
      }
    );
    // The Codex choice is offered only where it is already set or granted:
    // only Aokie's own older speech path sent text there, and Aokie has no
    // speech of its own now (calls go to OAIY), so nothing new is offered.
    return Promise.all([consentP, settingsP]).then(function () {
      loadingNow = false;
      if (isCodexLiveCallEndpoint(settingsBag.aiEndpoint)) {
        codexProviderAvailable = true;
        codexDestinationOptIn = true;
      }
      if (codexDestinationOptIn) codexProviderAvailable = true;
      render();
      reportSetup();
    });
  }

  /** The effective remote destinations this configuration would send data
   *  to. Generic loopback processing is local; the two exact Codex broker
   *  routes disclose their stable OpenAI destination instead of a local URL. */
  function uniqueDestinations() {
    // The plugin says where the saved settings send call data (consent.get
    // `destinations`), as it enforces it: this screen's own reading once
    // knew two of the four Codex routes and one of the two Desktop ports, so
    // the grant it issued missed what the plugin then refused. The reading
    // below is only for an older plugin that does not say.
    if (status && Array.isArray(status.destinations)) {
      return status.destinations.filter(function (d) {
        return typeof d === 'string' && d.length > 0;
      });
    }
    // (sttEndpoint and ttsEndpoint were Aokie's own speech servers: nothing
    // is sent to them now.)
    var keys = [
      'aiEndpoint',
      'audioTranscriptEndpoint',
      'realtimeVoiceEndpoint',
      'realtimeVoiceDestination',
    ];
    var out = [];
    for (var i = 0; i < keys.length; i++) {
      var raw = typeof settingsBag[keys[i]] === 'string' ? settingsBag[keys[i]].trim() : '';
      var v = effectiveDestination(raw);
      if (v.length > 0 && out.indexOf(v) === -1) out.push(v);
    }
    return out;
  }

  function consentDestinations() {
    var out = uniqueDestinations();
    // Inside OAIY, calls go to OAIY's own voice gateway on this computer,
    // named by this origin (the setup wizard's last step, Answer calls with
    // OAIY, points them there after this one has run). Granted with the rest,
    // so that step does not find it missing and pause the phone line.
    if (IN_OAIY && OAIY.destination && out.indexOf(OAIY.destination) === -1) out.push(OAIY.destination);
    if (codexDestinationOptIn && out.indexOf(CODEX_DESTINATION) === -1) {
      out.push(CODEX_DESTINATION);
    }
    return out;
  }

  // The operator was told a call on the line would end, and pressed Accept
  // again to go ahead. For that press only: cleared whenever the form opens,
  // closes or its choices change, and when the tab is left, so a warning given
  // for one call and one choice never waves a later Accept through.
  var callWarned = false;

  /** Accept: a grant without Bluetooth or transcription takes the phone line
   *  down at once (the plugin stops the radio, which ends a call on it), so
   *  with a call on the line that is said first, and a second press goes on. */
  /** What is wrong with the days typed for keeping records, '' when nothing. */
  function retentionProblem() {
    var t = String(retentionText).trim();
    if (!/^\d+$/.test(t)) return 'Enter a whole number of days, from 1 to ' + RETENTION_MAX + '.';
    var n = Number(t);
    if (n < 1) return 'Keep call records for at least 1 day.';
    if (n > RETENTION_MAX) return 'Keep call records for at most ' + RETENTION_MAX + ' days (10 years).';
    return '';
  }

  function accept() {
    if (submitting) return;
    // (days that are not 1 to 3650 are said by the field, not sent as 90)
    var bad = retentionProblem();
    if (bad) {
      error = 'Not recorded: ' + bad;
      render();
      var field = root && root.querySelector ? root.querySelector('#cns-retention') : null;
      if (field) field.focus();
      return;
    }
    retentionDays = Number(String(retentionText).trim());
    var takesLineDown = !scopes.bluetooth || !scopes.transcription;
    submitting = true;
    error = null;
    render();
    var liveCall =
      takesLineDown && !callWarned
        ? HOST.command('call.current').then(
            function (d) {
              var c = d && d.call;
              return !!(c && c.state && c.state !== 'ended');
            },
            function () {
              return false;
            }
          )
        : Promise.resolve(false);
    liveCall.then(function (live) {
      if (live) {
        submitting = false;
        callWarned = true;
        error =
          'A call is on the line, and without ' + (!scopes.bluetooth ? 'the Bluetooth phone link' : 'live call transcription') +
          ' the receptionist stops at once, which ends it. Press Accept & enforce again to go ahead, or wait for the call to end.';
        render();
        return;
      }
      callWarned = false;
      issueAndEnforce();
    });
  }

  function issueAndEnforce() {
    var grantScopes = {
      bluetooth: !!scopes.bluetooth,
      contacts: !!scopes.contacts,
      sms: !!scopes.sms,
      transcription: !!scopes.transcription,
      recording: !!scopes.recording,
      remoteCaptions: !!scopes.remoteCaptions,
      remoteAssistance: !!scopes.remoteAssistance,
      remoteMonitoring: !!scopes.remoteMonitoring,
      remoteConsult: !!scopes.remoteConsult,
      remoteTakeover: !!scopes.remoteTakeover,
      retentionDays: retentionDays,
      destinations: consentDestinations(),
    };
    var issued = null;
    HOST.consent
      .issue({
        version: (status && status.requiredVersion) || 1,
        scopes: grantScopes,
        expiresDays: 365,
      })
      .then(function (result) {
        issued = result || null;
        // Production posture from here on: enforcement, not warnings.
        return HOST.command('settings.set', { consentMode: 'enforce' });
      })
      .then(
        function (saved) {
          wizardOpen = false;
          submitting = false;
          // Recorded is not the same as running: the plugin may hold the
          // line paused (a destination not granted, transcription denied on
          // the OAIY route). Say which, from what it reports now.
          var said = function (v) {
            return v && typeof v.blocked === 'string' ? v.blocked.trim() : '';
          };
          var blockedNow = said(saved) || said(issued);
          return load().then(function () {
            var paused = (status && typeof status.blocked === 'string' && status.blocked) || blockedNow;
            if (paused) {
              HOST.toast(
                'error',
                'Consent recorded, but the phone line is paused: ' + U.plainTimes(paused) + '. The receptionist answers no calls until this is put right.'
              );
            } else {
              HOST.toast('success', 'Consent recorded and enforced: anything not allowed is refused.');
            }
          });
        },
        function (e) {
          error = errMsg(e);
          submitting = false;
          render();
        }
      );
  }

  // ---- rendering ----------------------------------------------------------

  /** How a consent.get status stands against this configuration. `ok` is
   *  the green "Consent vN · enforced" state. */
  function review(s) {
    var grant = s.grant || null;
    var enforced = s.mode === 'enforce';
    var destinationCurrent = false;
    if (grant && typeof s.destinationsCovered === 'boolean') {
      // The plugin's own verdict (consent.get), the one it enforces.
      destinationCurrent = s.destinationsCovered;
    } else if (grant && grant.scopes) {
      var granted = Array.isArray(grant.scopes.destinations) ? grant.scopes.destinations : [];
      var required = uniqueDestinations();
      destinationCurrent = true;
      for (var di = 0; di < required.length; di++) {
        var found = false;
        for (var gi = 0; gi < granted.length; gi++) {
          if (effectiveDestination(granted[gi]) === required[di]) {
            found = true;
            break;
          }
        }
        if (!found) {
          destinationCurrent = false;
          break;
        }
      }
    }
    // The plugin's gate for the phone line itself (consent.get `bluetooth`):
    // an expired grant, or one for an older version, is refused there before
    // the radio is stopped for it, so the badge read green over a grant the
    // plugin no longer honours. A grant that leaves Bluetooth out on purpose
    // is the person's choice, not a fault of the grant.
    var gate = s.bluetooth && typeof s.bluetooth === 'object' ? s.bluetooth : null;
    var lineRefused = !!(gate && gate.allowed === false && !(grant && grant.scopes && grant.scopes.bluetooth === false));
    // A radio paused for consent (`blocked`) needs action too: the badge
    // read green, "Consent v3 · enforced", while the phone line was off.
    var needsAction =
      !grant ||
      grant.version !== s.requiredVersion ||
      !enforced ||
      !destinationCurrent ||
      !!s.note ||
      !!s.blocked ||
      lineRefused;
    return {
      grant: grant,
      enforced: enforced,
      destinationCurrent: destinationCurrent,
      lineRefused: lineRefused,
      needsAction: needsAction,
      ok: !!grant && enforced && !needsAction,
    };
  }

  /** Setup mode: tell the wizard once consent is recorded and enforced (and
   *  nothing blocks the radio for consent). The first time it is not, open
   *  the consent form at once: that form is what this step is for. */
  function reportSetup() {
    var S = TABS.setup;
    if (!S || !S.active() || !status) return;
    var r = review(status);
    if (r.ok && !status.blocked) {
      S.done('Consent v' + r.grant.version + ' recorded and enforced');
      return;
    }
    S.unsatisfied();
    if (!setupFormOpened) {
      setupFormOpened = true;
      if (!wizardOpen) {
        wizardOpen = true;
        render();
      }
    }
  }

  /** A grant's date, as the person writes dates ('' when there is none). */
  function day(iso) {
    var t = iso ? new Date(iso) : null;
    return t && !isNaN(t.getTime()) ? t.toLocaleDateString(undefined, { day: 'numeric', month: 'short', year: 'numeric' }) : '';
  }

  /** Why the consent needs looking at, in a sentence. */
  function reviewReason(s, r) {
    var gate = s.bluetooth && typeof s.bluetooth === 'object' ? s.bluetooth : null;
    var said = s.note || s.blocked || (r.lineRefused && gate && gate.reason) || '';
    if (said) return U.sentence(said);
    if (!r.grant) return 'No consent is recorded on this computer yet. The receptionist answers no calls until it is.';
    if (r.grant.version !== s.requiredVersion) {
      return 'Consent version ' + r.grant.version + ' was accepted, but version ' + s.requiredVersion + ' is now needed.';
    }
    if (!r.destinationCurrent) return 'Call data now goes somewhere this consent does not name. Review it to accept the new destination.';
    if (!r.enforced) return 'Consent is recorded, but not enforced yet.';
    return '';
  }

  function statusCardHtml() {
    if (status === undefined) return U.loadingHtml;
    if (status === null) {
      return U.problemHtml(loadError || 'The consent could not be read.');
    }
    var r = review(status);
    var grant = r.grant;
    var needsAction = r.needsAction;

    var badge;
    if (grant && r.enforced && !needsAction) {
      badge =
        '<span class="rcp-badge is-ok" title="' +
        esc('Accepted ' + (grant.acceptedAt || '') + ' by ' + (grant.acceptedBy || 'operator') + ' · expires ' + (grant.expiresAt || '—')) +
        '">Consent v' + esc(grant.version) + ' · enforced</span>';
    } else {
      badge =
        '<span class="rcp-badge is-pending">' +
        (status.blocked || r.lineRefused
          ? 'Consent required — phone offline'
          : grant
            ? 'Consent needs review'
            : 'Consent not recorded') +
        '</span>';
    }
    var reason = needsAction ? reviewReason(status, r) : '';

    // What was granted, when, and until when.
    var facts = '';
    if (grant) {
      var scopes = grant.scopes || {};
      var allowed = [];
      for (var i = 0; i < SCOPE_ROWS.length; i++) {
        if (scopes[SCOPE_ROWS[i].key] === true) allowed.push(SCOPE_ROWS[i].label);
      }
      var expires = day(grant.expiresAt);
      var expired = !!grant.expiresAt && !isNaN(Date.parse(grant.expiresAt)) && Date.parse(grant.expiresAt) <= Date.now();
      var dests = Array.isArray(scopes.destinations) ? scopes.destinations.filter(function (d) { return typeof d === 'string' && d; }) : [];
      var ownOrigin = OAIY && OAIY.destination;
      var destNames = dests.map(function (d) {
        return d === ownOrigin ? 'OAIY on this computer (' + d + ')' : d;
      });
      // (two lists: the four short facts fill their row however wide it is)
      facts =
        '<dl class="rcp-facts">' +
        '<div><dt>Accepted</dt><dd>' + esc(day(grant.acceptedAt) || '—') + '</dd></div>' +
        '<div><dt>' + (expired ? 'Expired' : 'Expires') + '</dt><dd' + (expired ? ' class="is-warn"' : '') + '>' + esc(expires || 'Never') + '</dd></div>' +
        '<div><dt>Version</dt><dd>' + esc(grant.version) +
        (grant.version !== status.requiredVersion ? ' <small>(' + esc(status.requiredVersion) + ' needed)</small>' : '') + '</dd></div>' +
        '<div><dt>Records kept</dt><dd>' + esc(typeof scopes.retentionDays === 'number' ? scopes.retentionDays + ' days' : '—') + '</dd></div>' +
        '</dl>' +
        '<dl class="rcp-facts">' +
        '<div class="is-wide"><dt>Allows</dt><dd>' + esc(allowed.length ? allowed.join(', ') : 'Nothing') + '</dd></div>' +
        '<div class="is-wide"><dt>Call data goes to</dt><dd>' + esc(destNames.length ? destNames.join(', ') : 'This computer only') + '</dd></div>' +
        '</dl>';
    }
    var warnHint =
      status.mode === 'warn'
        ? '<p class="rcp-hint is-warn">Enforcement is off (a developer setting): calls are handled even without consent recorded.</p>'
        : '';
    return (
      '<div class="rcp-card__body">' +
      '<div class="rcp-actions rcp-consent__bar">' +
      badge +
      '<button type="button" class="rcp-button' + (needsAction && !wizardOpen ? ' is-primary' : '') + '" data-act="cns-toggle">' +
      (wizardOpen ? 'Close the form' : grant ? 'Review consent' : 'Set up consent') +
      '</button>' +
      '</div>' +
      (reason ? '<div class="rcp-callout is-warn" role="status">' + ICONS.alert + '<span>' + esc(reason) + '</span></div>' : '') +
      facts +
      warnHint +
      '</div>'
    );
  }

  function wizardHtml() {
    if (!wizardOpen) return '';
    var dests = consentDestinations();
    var codexConfigured = isCodexLiveCallEndpoint(settingsBag.aiEndpoint);
    var realtimeConfigured =
      settingsBag.realtimeVoiceMode === 'desktop_realtime' &&
      isDesktopRealtimeEndpoint(settingsBag.realtimeVoiceEndpoint) &&
      effectiveDestination(settingsBag.realtimeVoiceDestination).length > 0;
    var codexChoice = '';
    if (codexProviderAvailable) {
      codexChoice =
        '<label class="rcp-scope-row">' +
        '<input type="checkbox" data-codex-destination="1"' +
        (codexDestinationOptIn ? ' checked' : '') +
        (codexConfigured ? ' disabled' : '') +
        ' />' +
        '<span><strong>OpenAI ChatGPT via Codex</strong>' +
        '<span class="rcp-scope-detail">Allow caller transcript text to be processed by OpenAI through the signed-in Codex agent. ' +
        'Caller audio and account credentials are never included. Only Aokie’s own older speech path uses it; calls that go to OAIY do not.' +
        (codexConfigured ? ' Required by the current LLM source.' : '') +
        '</span></span></label>';
    }
    if (realtimeConfigured) {
      var realtimeDestination = effectiveDestination(settingsBag.realtimeVoiceDestination);
      if (OAIY && OAIY.callRoute(settingsBag) === 'oaiy') {
        // OAIY's own gateway: the audio goes to OAIY Desktop on this computer,
        // which names itself by this origin in the signed grant.
        codexChoice +=
          '<p class="rcp-notice" role="note"><strong>Call audio goes to OAIY:</strong> ' +
          'caller and receptionist audio streams to OAIY Desktop on this computer (<code>' +
          esc(realtimeDestination) + '</code>). OAIY Voice hears and speaks on this machine, and the Front desk ' +
          'agent writes the replies with the model set in OAIY. Calls need this destination.</p>';
      } else {
        codexChoice +=
          '<p class="rcp-notice rcp-notice--warn" role="note"><strong>Realtime call audio:</strong> ' +
          'the selected live-call voice provider sends raw caller and assistant audio through OAIY Desktop to ' +
          '<code>' + esc(realtimeDestination) + '</code>. The provider credential stays in Desktop, but the audio and transcripts are processed by that destination. ' +
          'This destination is required by the current Realtime voice mode.</p>';
      }
    }
    var scopeRows = [];
    for (var i = 0; i < SCOPE_ROWS.length; i++) {
      var row = SCOPE_ROWS[i];
      scopeRows.push(
        '<label class="rcp-scope-row">' +
          '<input type="checkbox" data-scope="' + row.key + '"' + (scopes[row.key] ? ' checked' : '') + ' />' +
          '<span><strong>' + esc(row.label) + '</strong>' +
          '<span class="rcp-scope-detail">' + esc(IN_OAIY && row.oaiyDetail ? row.oaiyDetail : row.detail) + '</span></span>' +
          '</label>'
      );
    }
    var destBlock;
    if (dests.length === 0) {
      destBlock =
        '<p class="rcp-hint">All call processing stays on this computer. With a Companion permission on, captions, ' +
        'status or encrypted call audio can also pass through the server your Companion apps connect through; it never ' +
        'receives the call unencrypted.</p>';
    } else {
      var items = [];
      for (var d = 0; d < dests.length; d++) {
        items.push(
          '<li><code>' + esc(dests[d]) + '</code>' +
            (OAIY && dests[d] === OAIY.destination ? ' — OAIY on this computer, which answers the calls' : '') +
            '</li>'
        );
      }
      destBlock =
        '<p class="rcp-hint">Call data (audio and transcripts) goes here to be processed. Sending it anywhere new later ' +
        'asks for consent again:</p>' +
        '<ul class="rcp-dest-list">' + items.join('') + '</ul>';
    }
    return (
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>Access &amp; consent</small>' +
      '<h3>What the receptionist may do</h3>' +
      '</div>' +
      '</div>' +
      '<div class="rcp-card__body">' +
      '<p class="rcp-hint">Version ' + esc((status && status.requiredVersion) || 1) +
      ' · consent lasts 12 months · ' +
      // Signed only where the host signs grants (consent.get signatureRequired); OAIY does not, and there the
      // grant is Aokie's own record on this computer, which a settings write cannot set aside once it is made.
      (status && status.signatureRequired === true
        ? "signed by this computer, so it can't be copied to another install. "
        : 'kept by Aokie on this computer; once recorded, no settings change can turn its enforcement off. ') +
      // (FormLogic's privacy page only where FormLogic Desktop is the host)
      (IN_OAIY
        ? 'Call audio and transcripts are handled on this computer, by Aokie and OAIY, and go nowhere else unless listed under Where call data goes.</p>'
        : 'Privacy and data handling: <code>formlogic.com/privacy</code></p>') +
      '<div class="rcp-scope-list">' + scopeRows.join('') + '</div>' +
      '<h4 class="rcp-subhead">Where call data goes</h4>' +
      codexChoice +
      destBlock +
      retentionHtml() +
      '<div class="rcp-callout is-warn">' + ICONS.alert +
      '<span><strong>Your callers, your responsibility:</strong> laws on call recording, transcription and AI ' +
      "disclosure differ from place to place. Many require you to tell callers they're speaking with an AI or " +
      'being transcribed — say so in your greeting. Check what applies to you before going live.</span></div>' +
      (error ? '<p class="rcp-error" role="alert">' + esc(error) + '</p>' : '') +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button is-primary" data-act="cns-accept"' + (submitting ? ' disabled' : '') + '>' +
      (submitting ? 'Recording…' : 'Accept & enforce') +
      '</button>' +
      '<button type="button" class="rcp-button" data-act="cns-close"' + (submitting ? ' disabled' : '') + '>Not now</button>' +
      '</div>' +
      '</div>' +
      '</section>'
    );
  }

  /** Days to keep call records: as typed, said at once when it is not 1 to 3650. */
  function retentionHtml() {
    var bad = retentionProblem();
    return (
      '<label class="rcp-field rcp-field--short"><span>Keep call records for (days)</span>' +
      '<input type="number" id="cns-retention" min="1" max="' + RETENTION_MAX + '" step="1" inputmode="numeric" required' +
      ' aria-describedby="cns-retention-note"' + (bad ? ' aria-invalid="true"' : '') + ' value="' + esc(retentionText) + '" /></label>' +
      '<p class="' + (bad ? 'rcp-error' : 'rcp-hint') + '" id="cns-retention-note">' +
      esc(bad || 'From 1 day to ' + RETENTION_MAX + ' (10 years).') +
      '</p>'
    );
  }

  /** The days' note and mark, as typed (the field is not drawn again). */
  function updateRetentionNote(input) {
    var bad = retentionProblem();
    var note = root && root.querySelector ? root.querySelector('#cns-retention-note') : null;
    if (note) {
      note.className = bad ? 'rcp-error' : 'rcp-hint';
      note.textContent = bad || 'From 1 day to ' + RETENTION_MAX + ' (10 years).';
    }
    if (input) {
      if (bad) input.setAttribute('aria-invalid', 'true');
      else input.removeAttribute('aria-invalid');
    }
  }

  /** One of the tab's two parts drawn again, only when it changed, keeping
   *  the control in use focused (the status is read every 10 s, while the
   *  form may be half filled in). */
  function drawZone(id, html) {
    var zone = root && root.querySelector('#' + id);
    if (!zone || zone.__cnsHtml === html) return;
    var at = U.focusOf(zone);
    zone.__cnsHtml = html;
    zone.innerHTML = html;
    U.restoreFocus(zone, at);
  }

  function render() {
    if (!root) return;
    var statusHtml =
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>Consent</small>' +
      '<h3>Consent for the phone receptionist</h3>' +
      '</div>' +
      '</div>' +
      statusCardHtml() +
      // (a read that failed after a good one: the status above is the last known)
      (status !== undefined && status !== null && loadError ? U.problemHtml(loadError) : '') +
      '</section>';
    // (a container that cannot be searched, as in the isolated checks, is drawn whole)
    if (typeof root.querySelector !== 'function') {
      root.innerHTML = statusHtml + wizardHtml();
      return;
    }
    if (!root.querySelector('#cns-status-zone')) {
      root.innerHTML = '<div class="rcp-zone" id="cns-status-zone"></div><div class="rcp-zone" id="cns-form-zone"></div>';
    }
    drawZone('cns-status-zone', statusHtml);
    drawZone('cns-form-zone', wizardHtml());
  }

  // ---- wiring -------------------------------------------------------------

  function wire(el) {
    if (el.__aokieConsentWired) return;
    el.__aokieConsentWired = true;
    el.addEventListener('click', function (e) {
      var btn = e.target && e.target.closest ? e.target.closest('[data-act]') : null;
      if (!btn || btn.disabled) return;
      var act = btn.getAttribute('data-act');
      if (act === 'cns-toggle') {
        wizardOpen = !wizardOpen;
        error = null;
        callWarned = false;
        render();
      } else if (act === 'cns-close') {
        wizardOpen = false;
        callWarned = false;
        render();
      } else if (act === 'cns-accept') accept();
    });
    el.addEventListener('change', function (e) {
      var t = e.target;
      if (!t || !t.getAttribute) return;
      var scope = t.getAttribute('data-scope');
      if (scope != null) {
        scopes[scope] = !!t.checked;
        callWarned = false;
        return;
      }
      if (t.getAttribute('data-codex-destination') != null) {
        codexDestinationOptIn = !!t.checked;
        render();
        return;
      }
      if (t.id === 'cns-retention') {
        retentionText = t.value;
        updateRetentionNote(t);
      }
    });
    // The days are checked as they are typed, not only when the field is left.
    el.addEventListener('input', function (e) {
      var t = e.target;
      if (t && t.id === 'cns-retention') {
        retentionText = t.value;
        updateRetentionNote(t);
      }
    });
  }

  TABS.register('consent', {
    mount: function (el) {
      root = el;
      wire(el);
      render();
      load();
      // Read again while the tab is open, as the other tabs do, so a pause
      // (an expired grant, a destination no longer covered) shows here too.
      // Not in OAIY's setup wizard, which checks the step itself.
      var S = TABS.setup;
      if (pollTimer == null && !(S && S.active())) {
        pollTimer = window.setInterval(function () {
          if (!document.hidden && root && !submitting && !loadingNow) load();
        }, 10000);
      }
    },
    unmount: function () {
      root = null;
      callWarned = false;
      if (pollTimer != null) {
        window.clearInterval(pollTimer);
        pollTimer = null;
      }
    },
  });
})();
