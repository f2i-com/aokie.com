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

  // The Desktop is a loopback broker, but these two reserved provider routes
  // delegate transcript text to OpenAI. They therefore share one stable,
  // account-free effective destination in the signed consent grant. Every
  // other loopback route remains local.
  var CODEX_DESTINATION = 'OpenAI ChatGPT via Codex';
  var CODEX_ID_NONE = 'openai-codex-agent-none';
  var CODEX_ID_LOW = 'openai-codex-agent-low';
  var CODEX_PROVIDER_NONE = 'provider:' + CODEX_ID_NONE;
  var CODEX_PROVIDER_LOW = 'provider:' + CODEX_ID_LOW;

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

  function effectiveDestination(raw) {
    var value = String(raw || '').trim();
    if (!value) return '';
    if (isCodexLiveCallEndpoint(value)) return CODEX_DESTINATION;
    if (value === CODEX_DESTINATION) return CODEX_DESTINATION;
    if (isLoopbackUrl(value)) return '';
    try {
      return new URL(value).origin;
    } catch (e) {
      return value;
    }
  }

  function isCodexProviderSource(source) {
    return !!source && (source.id === CODEX_PROVIDER_NONE || source.id === CODEX_PROVIDER_LOW);
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
  var retentionDays = 90;
  var wizardOpen = false;
  var submitting = false;
  var error = null;
  var codexProviderAvailable = false;
  var codexDestinationOptIn = false;
  var destinationSeeded = false;

  for (var i = 0; i < SCOPE_ROWS.length; i++) scopes[SCOPE_ROWS[i].key] = SCOPE_ROWS[i].defaultOn;

  function load() {
    var consentP = HOST.consent.get().then(
      function (s) {
        status = s || null;
        // A previous load's failure must not haunt a now-healthy status
        // readout (accept-flow errors render while the wizard is open and
        // accept() manages its own error slot before calling load()).
        error = null;
        // Re-consent starts from what was previously granted.
        if (!scopesSeeded && s && s.grant && s.grant.scopes) {
          for (var i = 0; i < SCOPE_ROWS.length; i++) {
            var key = SCOPE_ROWS[i].key;
            if (typeof s.grant.scopes[key] === 'boolean') scopes[key] = s.grant.scopes[key];
          }
          scopesSeeded = true;
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
        error = errMsg(e);
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
    var sourcesP = HOST.aiSources().then(
      function (list) {
        var sources = Array.isArray(list) ? list : [];
        for (var i = 0; i < sources.length; i++) {
          if (isCodexProviderSource(sources[i])) {
            codexProviderAvailable = true;
            break;
          }
        }
      },
      function () {
        // A source-list outage must not hide an already configured or granted
        // disclosure choice; the reconciliation below preserves those.
      }
    );
    return Promise.all([consentP, settingsP, sourcesP]).then(function () {
      if (isCodexLiveCallEndpoint(settingsBag.aiEndpoint)) {
        codexProviderAvailable = true;
        codexDestinationOptIn = true;
      }
      if (codexDestinationOptIn) codexProviderAvailable = true;
      render();
    });
  }

  /** The effective remote destinations this configuration would send data
   *  to. Generic loopback processing is local; the two exact Codex broker
   *  routes disclose their stable OpenAI destination instead of a local URL. */
  function uniqueDestinations() {
    var keys = ['aiEndpoint', 'sttEndpoint', 'ttsEndpoint', 'audioTranscriptEndpoint'];
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
    if (codexDestinationOptIn && out.indexOf(CODEX_DESTINATION) === -1) {
      out.push(CODEX_DESTINATION);
    }
    return out;
  }

  function accept() {
    if (submitting) return;
    submitting = true;
    error = null;
    render();
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
    HOST.consent
      .issue({
        version: (status && status.requiredVersion) || 1,
        scopes: grantScopes,
        expiresDays: 365,
      })
      .then(function () {
        // Production posture from here on: enforcement, not warnings.
        return HOST.command('settings.set', { consentMode: 'enforce' });
      })
      .then(
        function () {
          HOST.toast(
            'success',
            'Consent recorded — enforcement is on; denied scopes are refused at the point of capture.'
          );
          wizardOpen = false;
          submitting = false;
          return load();
        },
        function (e) {
          error = errMsg(e);
          submitting = false;
          render();
        }
      );
  }

  // ---- rendering ----------------------------------------------------------

  function statusCardHtml() {
    if (status === undefined) return '<p class="rcp-loading">Loading…</p>';
    if (status === null) {
      return (
        '<p class="rcp-error">' +
        esc(error || 'consent.get failed — is the plugin running?') +
        '</p>'
      );
    }
    var grant = status.grant || null;
    var enforced = status.mode === 'enforce';
    var destinationCurrent = false;
    if (grant && grant.scopes) {
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
    var needsAction =
      !grant ||
      grant.version !== status.requiredVersion ||
      !enforced ||
      !destinationCurrent ||
      !!status.note;

    var badge;
    if (grant && enforced && !needsAction) {
      badge =
        '<span class="rcp-badge is-ok" title="' +
        esc('Accepted ' + (grant.acceptedAt || '') + ' by ' + (grant.acceptedBy || 'operator') + ' · expires ' + (grant.expiresAt || '—')) +
        '">Consent v' + esc(grant.version) + ' · enforced</span>';
    } else {
      badge =
        '<span class="rcp-badge is-pending" title="' +
        esc(
          status.note ||
            status.blocked ||
            (!destinationCurrent && grant
              ? 'A configured data destination has not been accepted yet.'
              : 'Consent has not been recorded on this device.')
        ) +
        '">' +
        (status.blocked
          ? 'Consent required — phone offline'
          : grant
            ? 'Consent needs review'
            : 'Consent not recorded') +
        '</span>';
    }
    var warnHint =
      status.mode === 'warn'
        ? '<p class="rcp-hint">warn mode (developer override) — sensitive processing runs with consent unrecorded</p>'
        : '';
    return (
      '<div class="rcp-card__body">' +
      '<div class="rcp-actions" style="margin-top: 0; justify-content: space-between;">' +
      '<span>' + badge + '</span>' +
      '<button type="button" class="rcp-button' + (grant ? '' : ' is-primary') + '" data-act="cns-toggle">' +
      (wizardOpen ? 'Hide' : grant ? 'Review consent' : 'Set up consent') +
      '</button>' +
      '</div>' +
      warnHint +
      '</div>'
    );
  }

  function wizardHtml() {
    if (!wizardOpen) return '';
    var dests = consentDestinations();
    var codexConfigured = isCodexLiveCallEndpoint(settingsBag.aiEndpoint);
    var codexChoice = '';
    if (codexProviderAvailable) {
      codexChoice =
        '<label class="rcp-scope-row" style="margin-top: 8px;">' +
        '<input type="checkbox" data-codex-destination="1"' +
        (codexDestinationOptIn ? ' checked' : '') +
        (codexConfigured ? ' disabled' : '') +
        ' />' +
        '<span><strong>OpenAI ChatGPT via Codex</strong>' +
        '<span class="rcp-scope-detail">Allow caller transcript text to be processed by OpenAI through the signed-in Codex agent. ' +
        'Caller audio and account credentials are never included. Select this before choosing a ChatGPT via Codex live-call model.' +
        (codexConfigured ? ' Required by the current LLM source.' : '') +
        '</span></span></label>';
    }
    var scopeRows = [];
    for (var i = 0; i < SCOPE_ROWS.length; i++) {
      var row = SCOPE_ROWS[i];
      scopeRows.push(
        '<label class="rcp-scope-row">' +
          '<input type="checkbox" data-scope="' + row.key + '"' + (scopes[row.key] ? ' checked' : '') + ' />' +
          '<span><strong>' + esc(row.label) + '</strong>' +
          '<span class="rcp-scope-detail">' + esc(row.detail) + '</span></span>' +
          '</label>'
      );
    }
    var destBlock;
    if (dests.length === 0) {
      destBlock =
        '<p class="rcp-hint">All speech-processing endpoints are local to this machine. If a Companion scope is ' +
        'enabled, authorised state, captions or endpoint-encrypted WebRTC media can still travel through the ' +
        'selected FormLogic or custom deployment. The signalling server never receives unencrypted call audio; ' +
        "TURN can only relay encrypted packets. Linked FormLogic records follow each form's own retention settings.</p>";
    } else {
      var items = [];
      for (var d = 0; d < dests.length; d++) {
        items.push('<li><code>' + esc(dests[d]) + '</code></li>');
      }
      destBlock =
        '<p class="rcp-hint">These configured endpoints will receive call data (transcripts/audio for processing). ' +
        'Changing them later to somewhere new requires re-consent:</p>' +
        '<ul class="rcp-dest-list">' + items.join('') + '</ul>';
    }
    return (
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>ACCESS &amp; CONSENT</small>' +
      '<h3>Phone receptionist — access &amp; consent</h3>' +
      '</div>' +
      '</div>' +
      '<div class="rcp-card__body">' +
      '<p class="rcp-hint" style="margin-top: 0;">Version ' + esc((status && status.requiredVersion) || 1) +
      " · grants expire after 12 months · signed by this computer, so it can't be copied to another install. " +
      'Privacy &amp; data-handling disclosure: <code>formlogic.com/privacy</code></p>' +
      '<div style="margin-top: 8px;">' + scopeRows.join('') + '</div>' +
      '<div style="margin-top: 12px;">' +
      '<strong style="font-size: 11px;">Where call data goes</strong>' +
      codexChoice +
      destBlock +
      '</div>' +
      '<label class="rcp-field" style="max-width: 260px;"><span>Record retention (days)</span>' +
      '<input type="number" id="cns-retention" min="1" max="3650" value="' + esc(retentionDays) + '" /></label>' +
      '<div class="rcp-callout is-warn" style="margin-top: 12px;">' + ICONS.alert +
      '<span><strong>Your callers, your responsibility:</strong> laws on call recording, transcription and AI ' +
      "disclosure differ by jurisdiction. Many require you to TELL callers they're speaking with an AI and/or " +
      'being transcribed — put it in your greeting. Confirm your local requirements before going live.</span></div>' +
      (error ? '<p class="rcp-error" style="padding: 10px 0 0;">' + esc(error) + '</p>' : '') +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button" data-act="cns-close"' + (submitting ? ' disabled' : '') + '>Not now</button>' +
      '<button type="button" class="rcp-button is-primary" data-act="cns-accept"' + (submitting ? ' disabled' : '') + '>' +
      (submitting ? 'Recording…' : 'Accept & enforce') +
      '</button>' +
      '</div>' +
      '</div>' +
      '</section>'
    );
  }

  function render() {
    if (!root) return;
    root.innerHTML =
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>CONSENT</small>' +
      '<h3>Operator consent &amp; enforcement</h3>' +
      '</div>' +
      '</div>' +
      statusCardHtml() +
      (!wizardOpen && status !== undefined && status !== null && error
        ? '<p class="rcp-error">' + esc(error) + '</p>'
        : '') +
      '</section>' +
      wizardHtml();
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
        render();
      } else if (act === 'cns-close') {
        wizardOpen = false;
        render();
      } else if (act === 'cns-accept') accept();
    });
    el.addEventListener('change', function (e) {
      var t = e.target;
      if (!t || !t.getAttribute) return;
      var scope = t.getAttribute('data-scope');
      if (scope != null) {
        scopes[scope] = !!t.checked;
        return;
      }
      if (t.getAttribute('data-codex-destination') != null) {
        codexDestinationOptIn = !!t.checked;
        render();
        return;
      }
      if (t.id === 'cns-retention') {
        retentionDays = Math.max(1, Number(t.value) || 90);
      }
    });
  }

  TABS.register('consent', {
    mount: function (el) {
      root = el;
      wire(el);
      render();
      load();
    },
    unmount: function () {
      root = null;
    },
  });
})();
