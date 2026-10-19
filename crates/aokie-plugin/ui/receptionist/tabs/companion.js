/*
 * Companion tab — owner-confirmed enrollment for the Companion media endpoint.
 *
 * Faithful port of FormLogic Desktop's compiled CompanionPairingPanel
 * (desktop/src/aokie/CompanionPairingPanel.tsx) + the shared copy/helpers in
 * companionPairingUi.ts (names/semantics preserved). All calls ride
 * PluginHost.companionPairing (host-gated to the broker plugin's own screens).
 *
 * The Companion is the microphone/speaker WebRTC endpoint (the Android/iOS or
 * Windows companion app). It never opens the Bluetooth dongle.
 *
 * Sandbox deltas vs the compiled panel (noted honestly):
 *  - the desktop connection id can't be pre-filled (the bridge exposes no
 *    formlogic.getConfig) — the field keeps the compiled placeholder;
 *  - clipboard access may be refused in the opaque-origin iframe — a failed
 *    copy reveals the payload in a selectable textarea instead;
 *  - destructive confirms are INLINE (no host dialog): Approve shows the
 *    fingerprint to compare, Revoke shows the compiled dialog copy, and
 *    Rotate requires typing ROTATE.
 */
(function () {
  'use strict';

  var HOST = window.PluginHost;
  var TABS = window.__aokieTabs;
  if (!HOST || !TABS) return;
  var U = TABS.util;
  var esc = U.esc;
  var errMsg = U.errMsg;
  var svg = U.svg;
  var ICONS = U.icons;

  // ---- ported copy/helpers — desktop/src/aokie/companionPairingUi.ts ------

  var COMPANION_ENROLLMENT_HEADING = 'Connect a Companion app';

  // (Said plainly here: the compiled panel's words were for developers —
  // WebRTC, TURN packets, decoded PCM, admission bearers.)
  var COMPANION_TOPOLOGY_COPY =
    'Aokie takes the caller’s audio from the phone over Bluetooth, and this computer passes it, encrypted, to an approved Companion app, which plays it on that device’s own speaker and microphone. Here you pair Companion apps and decide which to trust; no audio hardware is paired here.';

  var COMPANION_MEDIA_PRIVACY_COPY =
    'The server that connects the two only passes messages between them, and may relay the encrypted audio; it can never hear the call.';

  function shortEndpointThumbprint(value) {
    if (!value) return '-';
    return value.length > 22 ? value.slice(0, 11) + '...' + value.slice(-9) : value;
  }

  /** Security comparisons must render the complete public thumbprint. */
  function fullEndpointThumbprint(value) {
    return value || '-';
  }

  /** The card's pill: ready (an app approved and this computer's key at
   *  hand), approved but not ready (the key is not), or none approved. It
   *  said "No app approved yet" beside "Apps approved: 1" when the key was
   *  missing. */
  function companionPairingSummary(status) {
    var approved = (status && status.approvedMobiles && status.approvedMobiles.length) || 0;
    var ready = !!(status && status.remoteAccessReady);
    return {
      label: ready ? 'Ready' : approved > 0 ? (approved === 1 ? '1 app approved' : approved + ' apps approved') + ' · not ready' : 'No app approved yet',
      tone: ready ? 'is-ok' : approved > 0 ? 'is-warn' : 'is-neutral',
      canGenerateOffer: !status || status.available !== false,
    };
  }

  // ---- state --------------------------------------------------------------

  var root = null;
  var status = undefined; // undefined = loading, null = failed, else the status
  var offer = null;
  var error = null; // the status read's own failure (the next good read clears it)
  // An action's failure, by the card its button is on, until the next action
  // there or Dismiss: 'key' (new key), 'offer' (pairing QR), 'reply' (a
  // pasted reply), 'apps' (approve, deny, remove). The status poll every
  // 3 s once cleared them with its own error slot, before they could be read.
  var actionErrors = { key: null, offer: null, reply: null, apps: null };
  var busy = false;
  var form = { appId: '', workspaceId: '', connectionId: '', responseJson: '' };
  var approveArm = null; // pending-approval id with the inline confirm open
  var revokeArm = null; // approved-mobile thumbprint with the inline confirm open
  var rotateOpen = false;
  var rotateText = '';
  var showPayloadText = false; // clipboard fallback: reveal the payload textarea
  var pollTimer = null;

  var smartphoneIcon = svg('<rect width="14" height="20" x="5" y="2" rx="2" ry="2"/><path d="M12 18h.01"/>', 16);

  function refresh() {
    return HOST.companionPairing.status().then(
      function (next) {
        status = next || null;
        error = null;
        renderDynamic();
      },
      function (e) {
        if (status === undefined) status = null;
        error = errMsg(e);
        renderDynamic();
      }
    );
  }

  // ---- actions ------------------------------------------------------------

  /** An action starts: its card's last failure is dropped. */
  function startAction(card) {
    busy = true;
    actionErrors[card] = null;
    renderDynamic();
  }

  /** An action failed: said on its card, by its button. */
  function actionFailed(card, e) {
    busy = false;
    actionErrors[card] = errMsg(e);
    renderDynamic();
  }

  function createOffer() {
    startAction('offer');
    HOST.companionPairing
      .createOffer({
        appId: form.appId.trim() || undefined,
        workspaceId: form.workspaceId.trim() || undefined,
        desktopConnectionId: form.connectionId.trim() || undefined,
      })
      .then(
        function (next) {
          offer = next || null;
          showPayloadText = false;
          HOST.toast('success', 'Pairing QR ready: scan it with the Companion app within 10 minutes.');
        },
        function (e) {
          actionErrors.offer = errMsg(e);
        }
      )
      .then(function () {
        busy = false;
        renderOffer();
        renderDynamic();
      });
  }

  function copyPayload() {
    if (!offer) return;
    var payload = offer.encodedPayload;
    var write;
    try {
      var clip = navigator.clipboard;
      write = clip && clip.writeText ? clip.writeText(payload) : Promise.reject(new Error('Clipboard unavailable'));
    } catch (e) {
      // Some engines THROW (not reject) on clipboard access from an
      // opaque-origin iframe — route it into the same fallback.
      write = Promise.reject(e);
    }
    write.then(
      function () {
        HOST.toast('success', 'Pairing text copied.');
      },
      function () {
        // Opaque-origin sandbox: clipboard access can be refused — fall back
        // to a selectable textarea instead of failing silently.
        showPayloadText = true;
        renderOffer();
        HOST.toast('info', 'This screen cannot copy for you: select the pairing text below and copy it yourself.');
      }
    );
  }

  function submitResponse() {
    var raw = form.responseJson.trim();
    if (raw === '') return;
    startAction('reply');
    var parsed;
    try {
      parsed = JSON.parse(raw);
    } catch (e) {
      actionFailed('reply', 'That is not a Companion reply (it should start with {"kind":"aokie_mobile_pairing_response"): ' + errMsg(e));
      return;
    }
    HOST.companionPairing.receiveResponse(parsed).then(
      function () {
        form.responseJson = '';
        var ta = root && root.querySelector('#cmp-response');
        if (ta) ta.value = '';
        HOST.toast('success', 'Reply checked: compare its fingerprint with the Companion, then approve it.');
        busy = false;
        refresh();
      },
      function (e) {
        actionFailed('reply', e);
      }
    );
  }

  function approve(id) {
    approveArm = null;
    startAction('apps');
    HOST.companionPairing.approve(id).then(
      function () {
        HOST.toast('success', 'Companion app approved.');
        busy = false;
        refresh();
      },
      function (e) {
        actionFailed('apps', e);
      }
    );
  }

  function deny(id) {
    startAction('apps');
    HOST.companionPairing.deny(id).then(
      function () {
        busy = false;
        refresh();
      },
      function (e) {
        actionFailed('apps', e);
      }
    );
  }

  function revoke(thumbprint) {
    revokeArm = null;
    startAction('apps');
    HOST.companionPairing.revoke(thumbprint).then(
      function () {
        HOST.toast('success', 'Companion app removed.');
        busy = false;
        refresh();
      },
      function (e) {
        actionFailed('apps', e);
      }
    );
  }

  function rotate() {
    rotateOpen = false;
    rotateText = '';
    startAction('key');
    HOST.companionPairing.rotateDesktopKey().then(
      function () {
        offer = null;
        renderOffer();
        HOST.toast('success', 'New key made for this computer: pair each Companion app again.');
        busy = false;
        refresh();
      },
      function (e) {
        actionFailed('key', e);
      }
    );
  }

  // ---- rendering ----------------------------------------------------------
  // The shell (headings, form inputs, relay textarea) renders ONCE per mount
  // so typing survives the 3 s status poll; the dynamic zones (pill, identity
  // grid, lists, offer, errors) re-render via innerHTML with change guards.
  // Three cards: this computer's identity (and its key), pairing a new app,
  // and the apps waiting for approval or approved.

  function cardHead(label, title, extra) {
    return (
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>' + label + '</small>' +
      '<h3>' + title + '</h3>' +
      '</div>' +
      (extra || '') +
      '</div>'
    );
  }

  function renderShell() {
    root.innerHTML =
      '<section class="rcp-card rcp-wide">' +
      cardHead(
        'Companion app',
        esc(COMPANION_ENROLLMENT_HEADING),
        '<span id="cmp-pill" class="rcp-pill is-neutral"><i></i><span id="cmp-pill-text">Checking</span></span>'
      ) +
      '<div class="rcp-card__body">' +
      '<p class="rcp-inline-note">' + esc(COMPANION_TOPOLOGY_COPY) + '</p>' +
      '<div class="rcp-callout">' + smartphoneIcon + '<span>' + esc(COMPANION_MEDIA_PRIVACY_COPY) +
      ' A pairing QR holds only public details and works once; it carries no password or key.</span></div>' +
      '<div id="cmp-identity"></div>' +
      '<div id="cmp-warning"></div>' +
      '<div id="cmp-error"></div>' +
      '<div id="cmp-rotate"></div>' +
      '<div id="cmp-key-error"></div>' +
      '</div>' +
      '</section>' +
      // Pairing a new app. The fields are static: typing must survive polls.
      '<section class="rcp-card">' +
      cardHead('Pair', 'Pair a new Companion app') +
      '<div class="rcp-card__body">' +
      '<p class="rcp-step-meta">Make a QR code here, then scan it with the Companion app on the device to pair. ' +
      'Each code works once, for 10 minutes. The app then shows up under Companion apps for you to approve.</p>' +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button is-primary" id="cmp-generate">Make a pairing QR</button>' +
      '</div>' +
      '<div id="cmp-offer-error"></div>' +
      '<div id="cmp-offer"></div>' +
      '<details class="rcp-details"><summary>Custom server (optional)</summary>' +
      '<p>Only for a Companion that connects through a server of your own. Leave these blank otherwise.</p>' +
      '<label class="rcp-field"><span>App id <small>(blank: Aokie’s own)</small></span>' +
      '<input type="text" class="is-mono" id="cmp-appid" placeholder="app identifier" /></label>' +
      '<label class="rcp-field"><span>Workspace id <small>(custom servers)</small></span>' +
      '<input type="text" class="is-mono" id="cmp-workspaceid" placeholder="workspace identifier" /></label>' +
      '<label class="rcp-field"><span>Desktop connection id <small>(a managed link, or a fixed local id)</small></span>' +
      '<input type="text" class="is-mono" id="cmp-connectionid" placeholder="connection identifier" /></label>' +
      '</details>' +
      '<details class="rcp-details"><summary>Paste a Companion’s reply</summary>' +
      '<p>For testing on this computer, or a server that does not pass the Companion’s reply on by itself: paste the signed reply here. ' +
      'It is checked in full (the app’s key, this computer, its one-use code and expiry) before it shows up for approval.</p>' +
      '<label class="rcp-field"><span>The Companion’s reply</span>' +
      '<textarea id="cmp-response" class="is-mono" rows="4" placeholder=\'{"kind":"aokie_mobile_pairing_response", ...}\'></textarea></label>' +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button" id="cmp-verify">Check the reply</button>' +
      '</div>' +
      '<div id="cmp-reply-error"></div>' +
      '</details>' +
      '</div>' +
      '</section>' +
      // The apps: waiting for approval, and approved.
      '<section class="rcp-card">' +
      cardHead('Apps', 'Companion apps') +
      '<div class="rcp-card__body">' +
      '<div id="cmp-apps-error"></div>' +
      '<div id="cmp-pending"></div>' +
      '<div id="cmp-approved"></div>' +
      '</div>' +
      '</section>';
    // Seed the sticky form values (they survive tab switches).
    root.querySelector('#cmp-appid').value = form.appId;
    root.querySelector('#cmp-workspaceid').value = form.workspaceId;
    root.querySelector('#cmp-connectionid').value = form.connectionId;
    root.querySelector('#cmp-response').value = form.responseJson;
    renderOffer();
    renderDynamic();
  }

  /** An action's failure on its card, with Dismiss. */
  function actionErrorHtml(card) {
    var msg = actionErrors[card];
    if (!msg) return '';
    return (
      '<div class="rcp-error rcp-action-error" role="alert"><span>' + U.esc(msg) + '</span>' +
      '<button type="button" class="rcp-link-btn" data-act="cmp-dismiss" data-card="' + card + '">Dismiss</button></div>'
    );
  }

  /** Draw a zone: nothing when it is unchanged, and the control in use
   *  keeps focus when it is not (U.setHtml). */
  function setZone(id, html) {
    var el = root && root.querySelector('#' + id);
    if (!el) return;
    U.setHtml(el, html);
  }

  /** Focus `sel` in the tab once it is drawn again (an inline confirm opened or closed). */
  function focusIn(sel) {
    var el = root && root.querySelector ? root.querySelector(sel) : null;
    if (el && typeof el.focus === 'function') el.focus();
  }

  function quoted(v) {
    return String(v || '').replace(/["\\]/g, '\\$&');
  }

  function renderOffer() {
    if (!root) return;
    var html = '';
    if (offer) {
      var qrSrc = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(offer.qrSvg || '');
      var expires = '';
      try {
        expires = new Date(offer.payload.expiresAt * 1000).toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
      } catch (e) {
        expires = '';
      }
      html =
        '<div class="rcp-qr">' +
        (offer.qrSvg ? '<img src="' + qrSrc.replace(/"/g, '&quot;') + '" alt="One-use Aokie Companion pairing QR" />' : '') +
        '<div class="rcp-qr__meta">' +
        '<strong>Scan this with the Companion app</strong>' +
        '<p>' + (expires ? 'It works once, until ' + esc(expires) + '.' : 'It works once.') + '</p>' +
        '<small>The Companion shows this computer’s key fingerprint: check all of it matches.</small>' +
        '<code class="rcp-code">' +
        esc(fullEndpointThumbprint(offer.payload && offer.payload.desktopEndpointKey && offer.payload.desktopEndpointKey.thumbprint)) +
        '</code>' +
        '<div class="rcp-actions"><button type="button" class="rcp-button is-small" data-act="cmp-copy">Copy as text</button></div>' +
        (showPayloadText
          ? '<textarea class="is-mono" rows="4" readonly aria-label="Pairing text">' + esc(offer.encodedPayload) + '</textarea>'
          : '') +
        '</div>' +
        '</div>';
    }
    setZone('cmp-offer', html);
  }

  function renderDynamic() {
    if (!root) return;
    var summary = companionPairingSummary(status || null);

    var pill = root.querySelector('#cmp-pill');
    var pillText = root.querySelector('#cmp-pill-text');
    if (pill && pillText) {
      if (status === undefined) {
        pill.className = 'rcp-pill is-neutral';
        pillText.textContent = 'Checking';
      } else if (status === null) {
        pill.className = 'rcp-pill is-warn';
        pillText.textContent = 'Unknown';
      } else {
        pill.className = 'rcp-pill ' + summary.tone;
        pillText.textContent = summary.label;
      }
    }

    // Identity grid.
    var idHtml = '';
    if (status === undefined) {
      idHtml = U.loadingHtml;
    } else if (status) {
      idHtml =
        '<div class="rcp-id-grid">' +
        '<span class="is-wide"><small>This computer’s key fingerprint</small><code>' +
        esc(fullEndpointThumbprint(status.endpointKey && status.endpointKey.thumbprint)) +
        '</code></span>' +
        '</div>' +
        // (a grid of their own, so three fill the row rather than leave a gap)
        '<div class="rcp-id-grid">' +
        '<span><small>Key kept in</small><strong>' + esc(status.protectionLabel || 'Not known') + '</strong></span>' +
        '<span><small>Apps approved</small><strong>' + esc((status.approvedMobiles || []).length) + '</strong></span>' +
        '<span><small>Trust list version</small><strong>' + esc(status.rosterRevision == null ? '—' : status.rosterRevision) + '</strong></span>' +
        '</div>';
    }
    setZone('cmp-identity', idHtml);

    setZone(
      'cmp-warning',
      status && status.warning
        ? '<div class="rcp-callout is-warn">' + ICONS.alert + '<span>' + esc(status.warning) + '</span></div>'
        : ''
    );
    setZone('cmp-error', error ? U.problemHtml(error) : '');
    for (var card in actionErrors) {
      if (Object.prototype.hasOwnProperty.call(actionErrors, card)) setZone('cmp-' + card + '-error', actionErrorHtml(card));
    }

    var gen = root.querySelector('#cmp-generate');
    if (gen) {
      gen.disabled = busy || !summary.canGenerateOffer;
      gen.textContent = offer ? 'Make a new QR' : 'Make a pairing QR';
    }
    var verify = root.querySelector('#cmp-verify');
    if (verify) verify.disabled = busy || form.responseJson.trim() === '';

    // Pending approvals.
    var pendHtml = '';
    var pending = (status && status.pendingApprovals) || [];
    if (pending.length > 0) {
      var rows = ['<h4>Waiting for your approval</h4>'];
      for (var i = 0; i < pending.length; i++) {
        var p = pending[i];
        rows.push(
          '<div class="rcp-person-row">' +
            '<span class="rcp-person-row__icon">' + smartphoneIcon + '</span>' +
            '<span class="rcp-person-row__id">' +
            '<strong>' + esc(p.displayName) + '</strong>' +
            '<small>' + esc(p.deviceId) + '</small>' +
            '<code>' + esc(p.fingerprint) + '</code>' +
            '</span>' +
            '<span class="rcp-person-row__actions">' +
            '<button type="button" class="rcp-button is-small" data-act="cmp-deny" data-id="' + esc(p.id) + '"' + (busy ? ' disabled' : '') + '>Deny</button>' +
            '<button type="button" class="rcp-button is-small is-primary" data-act="cmp-approve-arm" data-id="' + esc(p.id) + '"' + (busy || approveArm === p.id ? ' disabled' : '') + '>Approve…</button>' +
            '</span>' +
            (approveArm === p.id
              ? '<div class="rcp-confirm" role="group" aria-label="Approve this Companion app">' +
                '<p><strong>Approve ' + esc(p.displayName) + '?</strong> Check the Companion shows this same fingerprint first:</p>' +
                '<code class="rcp-code">' + esc(p.fingerprint) + '</code>' +
                '<div class="rcp-actions">' +
                '<button type="button" class="rcp-button is-primary" data-act="cmp-approve" data-id="' + esc(p.id) + '"' + (busy ? ' disabled' : '') + '>They match — approve</button>' +
                '<button type="button" class="rcp-button" data-act="cmp-approve-cancel" data-id="' + esc(p.id) + '">Cancel</button>' +
                '</div>' +
                '</div>'
              : '') +
            '</div>'
        );
      }
      pendHtml = '<div class="rcp-list">' + rows.join('') + '</div>';
    }
    setZone('cmp-pending', pendHtml);

    // Approved mobiles.
    var apprHtml = '';
    var approved = (status && status.approvedMobiles) || [];
    if (approved.length > 0) {
      var arows = ['<h4>Approved</h4>'];
      for (var a = 0; a < approved.length; a++) {
        var m = approved[a];
        var thumb = (m.endpointKey && m.endpointKey.thumbprint) || '';
        arows.push(
          '<div class="rcp-person-row">' +
            '<span class="rcp-person-row__icon is-ok">' + ICONS.check + '</span>' +
            '<span class="rcp-person-row__id">' +
            '<strong>' + esc(m.displayName) + '</strong>' +
            '<small>' + esc(m.deviceId) + ' · ' + esc(shortEndpointThumbprint(thumb)) + '</small>' +
            '</span>' +
            '<span class="rcp-person-row__actions">' +
            '<button type="button" class="rcp-button is-small is-danger" data-act="cmp-revoke-arm" data-thumb="' + esc(thumb) + '"' + (busy || revokeArm === thumb ? ' disabled' : '') + '>Remove…</button>' +
            '</span>' +
            (revokeArm === thumb
              ? '<div class="rcp-confirm is-danger" role="group" aria-label="Remove this Companion app">' +
                '<p><strong>Remove ' + esc(m.displayName) + '?</strong> It is cut off at once, from a call too, ' +
                'and can only connect again once it is paired and approved again.</p>' +
                '<div class="rcp-actions">' +
                '<button type="button" class="rcp-button is-danger" data-act="cmp-revoke" data-thumb="' + esc(thumb) + '"' + (busy ? ' disabled' : '') + '>Remove app</button>' +
                '<button type="button" class="rcp-button" data-act="cmp-revoke-cancel" data-thumb="' + esc(thumb) + '">Cancel</button>' +
                '</div>' +
                '</div>'
              : '') +
            '</div>'
        );
      }
      apprHtml = '<div class="rcp-list">' + arows.join('') + '</div>';
    } else if (status === undefined) {
      apprHtml = U.loadingHtml;
    } else if (!pending.length) {
      apprHtml =
        '<p class="rcp-empty">' +
        (status ? 'No Companion app is approved yet. Pair one, and it shows up here for you to approve.' : 'The apps show here once their list can be read.') +
        '</p>';
    }
    setZone('cmp-approved', apprHtml);

    // This computer's key: made new only after a suspected copy.
    // (not while the key's state cannot be read: a new key needs the one there is)
    var rotateDisabled = busy || !status || status.available === false;
    var rotHtml =
      '<div class="rcp-danger-row">' +
      '<span><strong>Make a new key for this computer</strong>' +
      '<small>Only if you think this computer’s key has been copied. Every Companion app must then be paired again.</small></span>' +
      '<button type="button" class="rcp-button is-danger" data-act="cmp-rotate-arm"' + (rotateDisabled || rotateOpen ? ' disabled' : '') + '>New key…</button>' +
      '</div>' +
      (rotateOpen
        ? '<div class="rcp-confirm is-danger" role="group" aria-label="Make a new key for this computer">' +
          '<p><strong>Make a new key for this computer?</strong> Every approved Companion app is removed here, ' +
          'and each must be paired and approved again: the old key is not handed on.</p>' +
          '<p>Type <strong>ROTATE</strong> to confirm:</p>' +
          '<div class="rcp-actions">' +
          // The input's live value is NOT baked into this HTML — the 3 s poll
          // re-renders unchanged markup, so typing keeps its focus; the input
          // handler drives the button's disabled state imperatively.
          '<input type="text" id="cmp-rotate-text" aria-label="Type ROTATE to confirm" autocomplete="off" />' +
          '<button type="button" class="rcp-button is-danger" data-act="cmp-rotate" disabled>Make a new key</button>' +
          '<button type="button" class="rcp-button" data-act="cmp-rotate-cancel">Cancel</button>' +
          '</div>' +
          '</div>'
        : '');
    setZone('cmp-rotate', rotHtml);
    // (the typed word is not in the markup: put back where the zone was drawn anew)
    var typed = root.querySelector('#cmp-rotate-text');
    if (typed && typed.value !== rotateText) typed.value = rotateText;
    var rotateBtn = root.querySelector('[data-act="cmp-rotate"]');
    if (rotateBtn) rotateBtn.disabled = !(rotateText.trim().toLowerCase() === 'rotate' && !busy);
  }

  // ---- wiring -------------------------------------------------------------

  function wire(el) {
    if (el.__aokieCompanionWired) return;
    el.__aokieCompanionWired = true;
    el.addEventListener('input', function (e) {
      var t = e.target;
      if (!t || !t.id) return;
      if (t.id === 'cmp-appid') form.appId = t.value;
      else if (t.id === 'cmp-workspaceid') form.workspaceId = t.value;
      else if (t.id === 'cmp-connectionid') form.connectionId = t.value;
      else if (t.id === 'cmp-response') {
        form.responseJson = t.value;
        var verify = root && root.querySelector('#cmp-verify');
        if (verify) verify.disabled = busy || form.responseJson.trim() === '';
      } else if (t.id === 'cmp-rotate-text') {
        rotateText = t.value;
        var btn = root && root.querySelector('[data-act="cmp-rotate"]');
        if (btn) btn.disabled = !(rotateText.trim().toLowerCase() === 'rotate' && !busy);
      }
    });
    el.addEventListener('click', function (e) {
      var t = e.target;
      if (t && t.id === 'cmp-generate') {
        createOffer();
        return;
      }
      if (t && t.id === 'cmp-verify') {
        submitResponse();
        return;
      }
      var btn = t && t.closest ? t.closest('[data-act]') : null;
      if (!btn || btn.disabled) return;
      var act = btn.getAttribute('data-act');
      if (act === 'cmp-dismiss') {
        actionErrors[btn.getAttribute('data-card')] = null;
        renderDynamic();
      } else if (act === 'cmp-copy') copyPayload();
      else if (act === 'cmp-deny') deny(btn.getAttribute('data-id'));
      else if (act === 'cmp-approve-arm') {
        approveArm = btn.getAttribute('data-id');
        renderDynamic();
        // (focus to the confirm: the button pressed is disabled while it is open)
        focusIn('[data-act="cmp-approve-cancel"]');
      } else if (act === 'cmp-approve') approve(btn.getAttribute('data-id'));
      else if (act === 'cmp-approve-cancel') {
        approveArm = null;
        renderDynamic();
        focusIn('[data-act="cmp-approve-arm"][data-id="' + quoted(btn.getAttribute('data-id')) + '"]');
      } else if (act === 'cmp-revoke-arm') {
        revokeArm = btn.getAttribute('data-thumb');
        renderDynamic();
        focusIn('[data-act="cmp-revoke-cancel"]');
      } else if (act === 'cmp-revoke') revoke(btn.getAttribute('data-thumb'));
      else if (act === 'cmp-revoke-cancel') {
        revokeArm = null;
        renderDynamic();
        focusIn('[data-act="cmp-revoke-arm"][data-thumb="' + quoted(btn.getAttribute('data-thumb')) + '"]');
      } else if (act === 'cmp-rotate-arm') {
        rotateOpen = true;
        rotateText = '';
        renderDynamic();
        focusIn('#cmp-rotate-text');
      } else if (act === 'cmp-rotate') rotate();
      else if (act === 'cmp-rotate-cancel') {
        rotateOpen = false;
        rotateText = '';
        renderDynamic();
        focusIn('[data-act="cmp-rotate-arm"]');
      }
    });
  }

  TABS.register('companion', {
    mount: function (el) {
      root = el;
      wire(el);
      renderShell();
      refresh();
      if (pollTimer == null) {
        pollTimer = window.setInterval(function () {
          if (!document.hidden && root) refresh();
        }, 3000);
      }
    },
    unmount: function () {
      root = null;
      if (pollTimer != null) {
        window.clearInterval(pollTimer);
        pollTimer = null;
      }
    },
  });
})();
