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

  var COMPANION_TOPOLOGY_COPY =
    'The Aokie plugin receives the caller audio from the Bluetooth phone link. Desktop bridges that audio over encrypted WebRTC to an approved Companion app, which uses its own microphone and speakers. This screen enrolls and trusts the app; it does not pair audio hardware.';

  var COMPANION_MEDIA_PRIVACY_COPY =
    'The server routes signalling and may relay encrypted TURN packets; it never receives decoded call PCM.';

  function shortEndpointThumbprint(value) {
    if (!value) return '-';
    return value.length > 22 ? value.slice(0, 11) + '...' + value.slice(-9) : value;
  }

  /** Security comparisons must render the complete public thumbprint. */
  function fullEndpointThumbprint(value) {
    return value || '-';
  }

  function companionPairingSummary(status) {
    return {
      label: status && status.remoteAccessReady ? 'Companion trusted' : 'Companion approval required',
      tone: status && status.remoteAccessReady ? 'is-ok' : 'is-neutral',
      canGenerateOffer: !status || status.available !== false,
    };
  }

  // ---- state --------------------------------------------------------------

  var root = null;
  var status = undefined; // undefined = loading, null = failed, else the status
  var offer = null;
  var error = null;
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

  function createOffer() {
    busy = true;
    error = null;
    renderDynamic();
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
          HOST.toast(
            'success',
            'Pairing QR ready — scan it in the Companion app. It expires in 10 minutes and contains no bearer token.'
          );
        },
        function (e) {
          error = errMsg(e);
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
    var clip = navigator.clipboard;
    var write = clip && clip.writeText ? clip.writeText(payload) : Promise.reject(new Error('Clipboard unavailable'));
    write.then(
      function () {
        HOST.toast('success', 'Pairing payload copied');
      },
      function () {
        // Opaque-origin sandbox: clipboard access can be refused — fall back
        // to a selectable textarea instead of failing silently.
        showPayloadText = true;
        renderOffer();
        HOST.toast('info', 'Clipboard unavailable in this sandboxed screen — select the payload below and copy it manually.');
      }
    );
  }

  function submitResponse() {
    var raw = form.responseJson.trim();
    if (raw === '') return;
    busy = true;
    error = null;
    renderDynamic();
    var parsed;
    try {
      parsed = JSON.parse(raw);
    } catch (e) {
      busy = false;
      error = 'That is not valid JSON: ' + errMsg(e);
      renderDynamic();
      return;
    }
    HOST.companionPairing.receiveResponse(parsed).then(
      function () {
        form.responseJson = '';
        var ta = root && root.querySelector('#cmp-response');
        if (ta) ta.value = '';
        HOST.toast('success', 'Mobile proof verified — compare its fingerprint, then approve it locally.');
        busy = false;
        refresh();
      },
      function (e) {
        busy = false;
        error = errMsg(e);
        renderDynamic();
      }
    );
  }

  function approve(id) {
    busy = true;
    approveArm = null;
    renderDynamic();
    HOST.companionPairing.approve(id).then(
      function () {
        HOST.toast('success', 'Companion endpoint approved');
        busy = false;
        refresh();
      },
      function (e) {
        busy = false;
        error = errMsg(e);
        renderDynamic();
      }
    );
  }

  function deny(id) {
    busy = true;
    renderDynamic();
    HOST.companionPairing.deny(id).then(
      function () {
        busy = false;
        refresh();
      },
      function (e) {
        busy = false;
        error = errMsg(e);
        renderDynamic();
      }
    );
  }

  function revoke(thumbprint) {
    busy = true;
    revokeArm = null;
    renderDynamic();
    HOST.companionPairing.revoke(thumbprint).then(
      function () {
        HOST.toast('success', 'Companion endpoint revoked');
        busy = false;
        refresh();
      },
      function (e) {
        busy = false;
        error = errMsg(e);
        renderDynamic();
      }
    );
  }

  function rotate() {
    busy = true;
    rotateOpen = false;
    rotateText = '';
    renderDynamic();
    HOST.companionPairing.rotateDesktopKey().then(
      function () {
        offer = null;
        renderOffer();
        HOST.toast('success', 'Desktop key rotated — pair each Companion again.');
        busy = false;
        refresh();
      },
      function (e) {
        busy = false;
        error = errMsg(e);
        renderDynamic();
      }
    );
  }

  // ---- rendering ----------------------------------------------------------
  // The shell (heading, form inputs, relay textarea) renders ONCE per mount
  // so typing survives the 3 s status poll; the dynamic zones (pill, identity
  // grid, lists, offer, errors) re-render via innerHTML with change guards.

  function renderShell() {
    root.innerHTML =
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>COMPANION DEVICE TRUST</small>' +
      '<h3>' + esc(COMPANION_ENROLLMENT_HEADING) + '</h3>' +
      '</div>' +
      '<span id="cmp-pill" class="rcp-pill is-neutral"><i></i><span id="cmp-pill-text">Checking</span></span>' +
      '</div>' +
      '<div class="rcp-card__body">' +
      '<p class="rcp-inline-note">' + esc(COMPANION_TOPOLOGY_COPY) + '</p>' +
      '<div class="rcp-callout">' + smartphoneIcon + '<span>' + esc(COMPANION_MEDIA_PRIVACY_COPY) +
      ' The QR contains public binding data plus a one-use expiry; no API key or admission bearer.</span></div>' +
      '<div id="cmp-identity"></div>' +
      '<div id="cmp-warning"></div>' +
      '<div id="cmp-error"></div>' +
      // Enrollment form (static — typing must survive polls).
      '<div class="rcp-form" style="padding: 0; margin-top: 12px;">' +
      '<label class="rcp-field" style="margin-top: 0;"><span>App id <small>(leave blank to use Aokie\'s assignment)</small></span>' +
      '<input type="text" id="cmp-appid" placeholder="app identifier" /></label>' +
      '<label class="rcp-field"><span>Workspace id <small>(optional/custom servers)</small></span>' +
      '<input type="text" id="cmp-workspaceid" placeholder="workspace identifier" /></label>' +
      '<label class="rcp-field"><span>Desktop connection id <small>(managed link or stable local id)</small></span>' +
      '<input type="text" id="cmp-connectionid" placeholder="filled from FormLogic when linked" /></label>' +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button is-primary" id="cmp-generate">Generate one-use enrollment QR</button>' +
      '</div>' +
      '</div>' +
      '<div id="cmp-offer"></div>' +
      '<div id="cmp-pending"></div>' +
      '<div id="cmp-approved"></div>' +
      '<details class="rcp-details"><summary>Custom relay / local testing</summary>' +
      '<p>Paste a signed mobile response here when a custom server or same-PC test app does not forward it automatically. ' +
      'Desktop still verifies the mobile key, app, Desktop binding, nonce, JTI and expiry before it appears above.</p>' +
      '<textarea id="cmp-response" class="rcp-textarea" rows="4" placeholder=\'{"kind":"aokie_mobile_pairing_response", ...}\'></textarea>' +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button" id="cmp-verify">Verify signed response</button>' +
      '</div>' +
      '</details>' +
      '<div id="cmp-rotate"></div>' +
      '</div>' +
      '<p class="rcp-footnote">Same-PC use is deliberately supported for development; production can relay the signed ' +
      'pairing response through FormLogic or a custom signalling service.</p>' +
      '</section>';
    // Seed the sticky form values (they survive tab switches).
    root.querySelector('#cmp-appid').value = form.appId;
    root.querySelector('#cmp-workspaceid').value = form.workspaceId;
    root.querySelector('#cmp-connectionid').value = form.connectionId;
    root.querySelector('#cmp-response').value = form.responseJson;
    renderOffer();
    renderDynamic();
  }

  function setZone(id, html) {
    var el = root && root.querySelector('#' + id);
    if (!el) return;
    if (el.__cmpHtml === html) return;
    el.__cmpHtml = html;
    el.innerHTML = html;
  }

  function renderOffer() {
    if (!root) return;
    var html = '';
    if (offer) {
      var qrSrc = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(offer.qrSvg || '');
      var expires = '';
      try {
        expires = new Date(offer.payload.expiresAt * 1000).toLocaleTimeString();
      } catch (e) {
        expires = '';
      }
      html =
        '<div class="rcp-qr">' +
        (offer.qrSvg ? '<img src="' + qrSrc.replace(/"/g, '&quot;') + '" alt="One-use Aokie Companion enrollment QR" />' : '') +
        '<div class="rcp-qr__meta">' +
        '<strong>Scan in the Companion app</strong>' +
        '<p>Expires ' + esc(expires) + '.</p>' +
        '<small>Desktop thumbprint - compare this complete value in Companion</small>' +
        '<code class="rcp-code">' +
        esc(fullEndpointThumbprint(offer.payload && offer.payload.desktopEndpointKey && offer.payload.desktopEndpointKey.thumbprint)) +
        '</code>' +
        '<div class="rcp-actions"><button type="button" class="rcp-button" data-act="cmp-copy">Copy JSON payload</button></div>' +
        (showPayloadText
          ? '<textarea class="rcp-textarea" rows="4" readonly style="width:100%;margin-top:8px;">' + esc(offer.encodedPayload) + '</textarea>'
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
      } else {
        pill.className = 'rcp-pill ' + (summary.tone === 'is-ok' ? 'is-ok' : 'is-neutral');
        pillText.textContent = summary.label;
      }
    }

    // Identity grid.
    var idHtml = '';
    if (status === undefined) {
      idHtml = '<p class="rcp-loading" style="padding: 12px 0 0;">Loading…</p>';
    } else if (status) {
      idHtml =
        '<div class="rcp-id-grid">' +
        '<span class="is-wide"><small>Desktop thumbprint (compare in full)</small><code>' +
        esc(fullEndpointThumbprint(status.endpointKey && status.endpointKey.thumbprint)) +
        '</code></span>' +
        '<span><small>Key protection</small><strong>' + esc(status.protectionLabel || 'Unavailable') + '</strong></span>' +
        '<span><small>Roster revision</small><strong>' + esc(status.rosterRevision) + '</strong></span>' +
        '<span><small>Approved endpoints</small><strong>' + esc((status.approvedMobiles || []).length) + '</strong></span>' +
        '</div>';
    }
    setZone('cmp-identity', idHtml);

    setZone(
      'cmp-warning',
      status && status.warning
        ? '<div class="rcp-callout is-warn">' + ICONS.alert + '<span>' + esc(status.warning) + '</span></div>'
        : ''
    );
    setZone('cmp-error', error ? '<p class="rcp-error" style="padding: 10px 0 0;">' + esc(error) + '</p>' : '');

    var gen = root.querySelector('#cmp-generate');
    if (gen) gen.disabled = busy || !summary.canGenerateOffer;
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
            '<button type="button" class="rcp-button" data-act="cmp-deny" data-id="' + esc(p.id) + '"' + (busy ? ' disabled' : '') + '>Deny</button>' +
            '<button type="button" class="rcp-button is-primary" data-act="cmp-approve-arm" data-id="' + esc(p.id) + '"' + (busy ? ' disabled' : '') + '>Approve</button>' +
            '</div>' +
            (approveArm === p.id
              ? '<div class="rcp-confirm">' +
                '<p><strong>Approve ' + esc(p.displayName) + '?</strong> Confirm this fingerprint on the Companion before approving:</p>' +
                '<code class="rcp-code" style="margin-bottom: 8px;">' + esc(p.fingerprint) + '</code>' +
                '<div class="rcp-actions">' +
                '<button type="button" class="rcp-button is-primary" data-act="cmp-approve" data-id="' + esc(p.id) + '"' + (busy ? ' disabled' : '') + '>Approve endpoint</button>' +
                '<button type="button" class="rcp-button" data-act="cmp-approve-cancel">Cancel</button>' +
                '</div>' +
                '</div>'
              : '')
        );
      }
      pendHtml = '<div class="rcp-list">' + rows.join('') + '</div>';
    }
    setZone('cmp-pending', pendHtml);

    // Approved mobiles.
    var apprHtml = '';
    var approved = (status && status.approvedMobiles) || [];
    if (approved.length > 0) {
      var arows = ['<h4>Approved Companion apps</h4>'];
      for (var a = 0; a < approved.length; a++) {
        var m = approved[a];
        var thumb = (m.endpointKey && m.endpointKey.thumbprint) || '';
        arows.push(
          '<div class="rcp-person-row">' +
            '<span class="rcp-person-row__icon is-ok">' + ICONS.check + '</span>' +
            '<span class="rcp-person-row__id">' +
            '<strong>' + esc(m.displayName) + '</strong>' +
            '<small>' + esc(m.deviceId) + ' / ' + esc(shortEndpointThumbprint(thumb)) + '</small>' +
            '</span>' +
            '<button type="button" class="rcp-button is-danger" data-act="cmp-revoke-arm" data-thumb="' + esc(thumb) + '"' + (busy ? ' disabled' : '') + '>Revoke</button>' +
            '</div>' +
            (revokeArm === thumb
              ? '<div class="rcp-confirm is-danger">' +
                '<p><strong>Revoke ' + esc(m.displayName) + '?</strong> Its current and future remote media admissions will be rejected. ' +
                'Reconnecting requires a fresh pairing ceremony.</p>' +
                '<div class="rcp-actions">' +
                '<button type="button" class="rcp-button is-danger" data-act="cmp-revoke" data-thumb="' + esc(thumb) + '"' + (busy ? ' disabled' : '') + '>Revoke endpoint</button>' +
                '<button type="button" class="rcp-button" data-act="cmp-revoke-cancel">Cancel</button>' +
                '</div>' +
                '</div>'
              : '')
        );
      }
      apprHtml = '<div class="rcp-list">' + arows.join('') + '</div>';
    }
    setZone('cmp-approved', apprHtml);

    // Rotate block.
    var rotateDisabled = busy || (status && status.available === false);
    var rotHtml =
      '<div class="rcp-person-row" style="border-top: 1px solid var(--line); margin-top: 12px;">' +
      '<span class="rcp-person-row__id">' +
      '<strong>Desktop endpoint key</strong>' +
      '<code style="font-family: inherit; color: var(--ink-faint);">Rotate only after suspected compromise. Every Companion app must enroll again.</code>' +
      '</span>' +
      '<button type="button" class="rcp-button is-danger" data-act="cmp-rotate-arm"' + (rotateDisabled ? ' disabled' : '') + '>Rotate key</button>' +
      '</div>' +
      (rotateOpen
        ? '<div class="rcp-confirm is-danger">' +
          '<p><strong>Rotate the Desktop endpoint key?</strong> All approved Companion endpoints will be revoked locally. ' +
          'This build intentionally requires fresh owner-confirmed pairing instead of accepting an unsigned key replacement.</p>' +
          '<p>Type <strong>ROTATE</strong> to confirm:</p>' +
          '<div class="rcp-actions">' +
          // The input's live value is NOT baked into this HTML — the 3 s poll
          // re-renders unchanged markup, so typing keeps its focus; the input
          // handler drives the button's disabled state imperatively.
          '<input type="text" id="cmp-rotate-text" class="rcp-input" style="min-height: 30px; padding: 0 10px; border: 1px solid var(--line); border-radius: 8px; background: var(--bg-0); color: var(--ink); font-size: 11.5px;" />' +
          '<button type="button" class="rcp-button is-danger" data-act="cmp-rotate" disabled>Rotate and revoke all</button>' +
          '<button type="button" class="rcp-button" data-act="cmp-rotate-cancel">Cancel</button>' +
          '</div>' +
          '</div>'
        : '');
    setZone('cmp-rotate', rotHtml);
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
      if (act === 'cmp-copy') copyPayload();
      else if (act === 'cmp-deny') deny(btn.getAttribute('data-id'));
      else if (act === 'cmp-approve-arm') {
        approveArm = btn.getAttribute('data-id');
        renderDynamic();
      } else if (act === 'cmp-approve') approve(btn.getAttribute('data-id'));
      else if (act === 'cmp-approve-cancel') {
        approveArm = null;
        renderDynamic();
      } else if (act === 'cmp-revoke-arm') {
        revokeArm = btn.getAttribute('data-thumb');
        renderDynamic();
      } else if (act === 'cmp-revoke') revoke(btn.getAttribute('data-thumb'));
      else if (act === 'cmp-revoke-cancel') {
        revokeArm = null;
        renderDynamic();
      } else if (act === 'cmp-rotate-arm') {
        rotateOpen = true;
        rotateText = '';
        renderDynamic();
      } else if (act === 'cmp-rotate') rotate();
      else if (act === 'cmp-rotate-cancel') {
        rotateOpen = false;
        rotateText = '';
        renderDynamic();
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
