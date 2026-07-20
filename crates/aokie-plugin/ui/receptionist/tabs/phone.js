/*
 * Phone setup tab — Bluetooth pairing + the bonded-phone roster.
 *
 * Faithful port of FormLogic Desktop's compiled PhonePairingControls
 * (desktop/src/aokie/AokieCard.tsx): the AOK-BT-001 bounded pairing window
 * (300 s countdown + Stop), the PAIR-001 SSP numeric-comparison confirm, and
 * the bonded list (Forget = `phone.removePaired`). The roster rows also get
 * Reconnect / Disconnect (`phone.connect` / `phone.disconnect`) — the
 * Overview tab keeps only a summary.
 *
 * ⚠️ PAIR-001 success rule (a live bug once): success = the bonded list GREW,
 * OR the operator accepted THIS window's numeric comparison AND the phone is
 * now connected. A bare connection is NEVER success — the ACL for a fresh
 * pairing comes up a beat BEFORE the numeric-comparison prompt, and closing
 * the window on "connected" slams it shut mid-SSP ("incorrect PIN or
 * passkey"). The confirm prompt stays visible while the plugin holds the SSP
 * reply (~25 s).
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

  /** Poll cadence while a pairing window is open (countdown + bond detection). */
  var PAIRING_POLL_MS = 2000;
  /** Gentle background cadence keeping the connected readout truthful at rest. */
  var IDLE_POLL_MS = 12000;
  /** New-phone pairing window: the plugin clamps to 30..=300 s; ask for the
   *  max — it auto-closes the moment one phone bonds and there's a live
   *  countdown + Stop button. */
  var PAIRING_WINDOW_SECONDS = 300;

  function formatSeconds(total) {
    var m = Math.floor(total / 60);
    var s = total % 60;
    return m + ':' + (s < 10 ? '0' : '') + s;
  }

  // ---- state (module-level — survives tab switches) -----------------------

  var root = null;
  var known = false; // first status+roster fetch landed
  var secondsLeft = 0;
  var connected = false;
  var device = null;
  var confirmPrompt = null; // { address, numericValue }
  var bonded = []; // [{ address, name, connected }]
  var busy = false;
  var error = null;
  var busyPhones = {};
  var forgetArm = null; // address with the inline Forget confirm open
  var baselineBonds = 0;
  var confirmed = false; // operator accepted THIS window's comparison
  var prevWindowOpen = false;
  var idleTimer = null;
  var windowTimer = null;
  var lastHtml = '';
  // Transport mode from settings.get — decides the pairing instructions
  // (native mode pairs in Windows Settings). The dongle copy is the default
  // until the first settings load lands (missing/unknown = dongle view).
  var transportMode = 'dongle';

  // ---- fetchers -----------------------------------------------------------

  function loadBonded() {
    return HOST.command('phone.listPaired').then(function (data) {
      var raw = (data && data.devices) || [];
      bonded = [];
      for (var i = 0; i < raw.length; i++) {
        var d = raw[i];
        if (typeof d === 'string') bonded.push({ address: d, name: null, connected: false });
        else if (d && d.address) bonded.push({ address: d.address, name: d.name || null, connected: !!d.connected });
      }
      return bonded;
    });
  }

  function pollStatus() {
    return HOST.command('phone.status').then(function (data) {
      var d = data || {};
      secondsLeft =
        d.pairingOpen && typeof d.pairingSecondsRemaining === 'number'
          ? d.pairingSecondsRemaining
          : 0;
      connected = !!d.connected || !!d.paired;
      device = d.device || null;
      // PAIR-001: surface (or clear) the numeric-comparison prompt. The
      // plugin reports it only while the radio actually holds the SSP reply.
      var pc = d.pairingConfirm;
      confirmPrompt =
        pc && pc.address && typeof pc.numericValue === 'number'
          ? { address: pc.address, numericValue: pc.numericValue }
          : null;
      return { secs: secondsLeft, connected: connected };
    });
  }

  function refreshAll() {
    return Promise.all([pollStatus(), loadBonded(), loadTransport()]).then(
      function () {
        known = true;
        error = null;
        afterPoll();
      },
      function (e) {
        error = errMsg(e);
        known = true;
        render();
      }
    );
  }

  // Best-effort transport fetch: a settings failure keeps the last-known
  // mode — it must never break the phone readouts. Also feeds the shared
  // transport truth in app.js (Dongle-tab visibility).
  function loadTransport() {
    return HOST.command('settings.get').then(
      function (data) {
        var bag = (data && data.settings) || {};
        if (TABS.transport && TABS.transport.update) TABS.transport.update(bag);
        transportMode = TABS.transport && TABS.transport.mode ? TABS.transport.mode() : 'dongle';
      },
      function () {
        /* keep the last-known mode */
      }
    );
  }

  // ---- window lifecycle ---------------------------------------------------

  /** React to poll results: baseline a freshly-opened window (also windows
   *  opened OUTSIDE this UI — flows, the command relay), run the success
   *  check while one is open, and keep the right timer running. */
  function afterPoll() {
    var windowOpen = secondsLeft > 0;
    if (windowOpen && !prevWindowOpen) {
      confirmed = false;
      baselineBonds = bonded.length;
    }
    prevWindowOpen = windowOpen;
    ensureTimers();
    render();
  }

  function windowTick() {
    Promise.all([pollStatus(), loadBonded()])
      .then(function () {
        var windowOpen = secondsLeft > 0;
        // A window that opened between ticks (flows / the relay) must
        // re-baseline BEFORE the success check, or a stale baseline could
        // fire a phantom success.
        if (windowOpen && !prevWindowOpen) {
          confirmed = false;
          baselineBonds = bonded.length;
        }
        prevWindowOpen = windowOpen;
        var success = bonded.length > baselineBonds || (confirmed && connected);
        if (success) {
          baselineBonds = bonded.length; // never re-fire for the same bond
          return HOST.command('phone.stopPairing')
            .catch(function () {
              /* the bond already auto-closed the window — fine */
            })
            .then(function () {
              secondsLeft = 0;
              prevWindowOpen = false;
              confirmed = false;
              HOST.toast(
                'success',
                'Phone connected — Aokie can take calls from this phone; it reconnects automatically from now on.'
              );
              return refreshAll();
            });
        }
        ensureTimers();
        render();
        return null;
      })
      .catch(function () {
        // Plugin stopping mid-poll — the idle poll recovers.
      });
  }

  function ensureTimers() {
    // root == null → the tab is unmounted: never (re)start the fast timer.
    // An in-flight poll that completes AFTER unmount lands here — without
    // the guard it would silently restart 2 s polling for an inactive tab.
    var wantFast = root != null && (secondsLeft > 0 || !!confirmPrompt);
    if (wantFast && windowTimer == null) {
      windowTimer = window.setInterval(function () {
        if (!document.hidden) windowTick();
      }, PAIRING_POLL_MS);
    } else if (!wantFast && windowTimer != null) {
      window.clearInterval(windowTimer);
      windowTimer = null;
    }
  }

  // ---- actions ------------------------------------------------------------

  function startPairing() {
    busy = true;
    error = null;
    render();
    // Pre-window baseline so the poller can spot the new bond.
    loadBonded()
      .catch(function () {
        return bonded;
      })
      .then(function () {
        baselineBonds = bonded.length;
        confirmed = false;
        return HOST.command('phone.startPairing', { seconds: PAIRING_WINDOW_SECONDS });
      })
      .then(
        function (d) {
          d = d || {};
          if (d.simulated) {
            HOST.toast('success', 'Simulated pairing session — dev mode has no radio, so no real pairing window was opened.');
            return;
          }
          secondsLeft = typeof d.windowSeconds === 'number' ? d.windowSeconds : PAIRING_WINDOW_SECONDS;
          prevWindowOpen = true;
          HOST.toast(
            'success',
            transportMode === 'dongle'
              ? 'Discoverable for 5 minutes — on your phone: Bluetooth → Pair new device → "Aokie AI Assistant".'
              : 'This PC is discoverable for 5 minutes — finish the pairing in Windows Settings → Bluetooth & devices → Add device.'
          );
        },
        function (e) {
          error = errMsg(e);
        }
      )
      .then(function () {
        busy = false;
        ensureTimers();
        render();
      });
  }

  function stopPairing() {
    busy = true;
    error = null;
    render();
    HOST.command('phone.stopPairing')
      .then(
        function () {
          secondsLeft = 0;
          prevWindowOpen = false;
        },
        function (e) {
          error = errMsg(e);
        }
      )
      .then(function () {
        busy = false;
        ensureTimers();
        render();
      });
  }

  // PAIR-001: answer the numeric comparison — the code shown here must match
  // the one on the phone's pairing dialog before the operator confirms.
  function resolvePairing(accept) {
    if (!confirmPrompt) return;
    var address = confirmPrompt.address;
    busy = true;
    error = null;
    render();
    HOST.command('phone.confirmPairing', { address: address, accept: accept })
      .then(
        function () {
          // Remember the operator accepted THIS window's comparison, so the
          // poller counts the phone reconnecting as success even when its
          // address was already stored (re-pair doesn't grow the bond count).
          if (accept) confirmed = true;
          confirmPrompt = null;
          if (!accept) {
            HOST.toast('success', 'Pairing refused — the phone was not paired. The window stays open if you want to try again.');
          }
        },
        function (e) {
          // Most common cause: the prompt expired (~25 s) before the click landed.
          confirmPrompt = null;
          error = errMsg(e);
        }
      )
      .then(function () {
        busy = false;
        ensureTimers();
        render();
      });
  }

  function doForget(address) {
    busy = true;
    error = null;
    forgetArm = null;
    render();
    HOST.command('phone.removePaired', { address: address })
      .then(
        function () {
          // The roster refresh is best-effort: a transient failure must not
          // reject through the busy-reset below (busy would stick forever).
          return loadBonded().catch(function () {
            /* the idle poll recovers the roster */
          });
        },
        function (e) {
          error = errMsg(e);
        }
      )
      .then(function () {
        busy = false;
        render();
      });
  }

  function phoneAct(command, address) {
    if (!address || busyPhones[address]) return;
    busyPhones[address] = true;
    render();
    HOST.command(command, { address: address })
      .then(
        function () {
          // Both commands are accepted-style: the radio finishes the work
          // asynchronously and the roster poll/events report the outcome.
          HOST.toast(
            'info',
            command === 'phone.connect'
              ? 'Reconnect requested for ' + address + ' — watching for the phone to come back.'
              : 'Disconnect requested for ' + address + ' — the phone may take a moment to drop.'
          );
          return refreshAll();
        },
        function (e) {
          HOST.toast(
            'error',
            (command === 'phone.connect' ? 'Reconnect failed: ' : 'Disconnect failed: ') + errMsg(e)
          );
        }
      )
      .then(function () {
        delete busyPhones[address];
        render();
      });
  }

  // ---- rendering ----------------------------------------------------------

  function pairingCardBody() {
    if (!known) return '<p class="rcp-loading">Loading…</p>';

    if (confirmPrompt) {
      return (
        '<div class="rcp-card__body">' +
        '<p class="rcp-step-meta">Phone <strong>' + esc(confirmPrompt.address) + '</strong> wants to pair. ' +
        'Confirm ONLY if your phone shows this same code:</p>' +
        '<div class="rcp-paircode">' + esc(String(confirmPrompt.numericValue).padStart(6, '0')) + '</div>' +
        '<div class="rcp-actions">' +
        '<button type="button" class="rcp-button is-primary" data-act="pair-confirm"' + (busy ? ' disabled' : '') + '>Codes match — pair</button>' +
        '<button type="button" class="rcp-button" data-act="pair-reject"' + (busy ? ' disabled' : '') + '>Reject</button>' +
        '</div>' +
        '</div>'
      );
    }

    if (secondsLeft > 0) {
      // Dongle mode: the phone pairs with the dongle itself. Native mode:
      // this window only makes the PC discoverable — Windows Settings owns
      // the actual pairing dialog and the code confirmation.
      var windowCopy =
        transportMode === 'dongle'
          ? 'Discoverable as “Aokie AI Assistant” — <strong>' + esc(formatSeconds(secondsLeft)) + '</strong> left. ' +
            'On your phone: Bluetooth → Pair new device, then confirm the matching code here.'
          : 'This PC is discoverable to nearby phones — <strong>' + esc(formatSeconds(secondsLeft)) + '</strong> left. ' +
            'Finish the pairing in Windows Settings → Bluetooth &amp; devices → Add device; Windows shows the pairing dialog and the code to confirm.';
      return (
        '<div class="rcp-card__body">' +
        '<p class="rcp-step-meta">' + windowCopy + '</p>' +
        '<div class="rcp-actions">' +
        '<button type="button" class="rcp-button" data-act="pair-stop"' + (busy ? ' disabled' : '') + '>Stop pairing</button>' +
        '</div>' +
        '</div>'
      );
    }

    if (connected) {
      return (
        '<div class="rcp-card__body">' +
        '<p class="rcp-status-ok">' + ICONS.check + ' Phone connected' +
        (device && device.address ? ' (' + esc(device.address) + ')' : '') +
        ' — Aokie can take calls.</p>' +
        '<div class="rcp-actions">' +
        '<button type="button" class="rcp-button" data-act="pair-start"' + (busy ? ' disabled' : '') + '>' +
        (busy ? 'Opening…' : 'Pair another phone') + '</button>' +
        '</div>' +
        '</div>'
      );
    }

    var idleCopy =
      transportMode === 'dongle'
        ? "New phones can't see the dongle until you open a pairing window " +
          '(already-paired phones reconnect on their own).'
        : 'Pair the phone in Windows Settings → Bluetooth &amp; devices → Add device — opening a pairing window here makes this PC discoverable while you do ' +
          '(already-paired phones reconnect on their own).';
    return (
      '<div class="rcp-card__body">' +
      '<p class="rcp-step-meta">' + idleCopy + '</p>' +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button is-primary" data-act="pair-start"' + (busy ? ' disabled' : '') + '>' +
      (busy ? 'Opening…' : 'Pair a phone') + '</button>' +
      '</div>' +
      '</div>'
    );
  }

  function bondedCardBody() {
    if (!known) return '<p class="rcp-loading">Loading…</p>';
    if (bonded.length === 0) {
      return transportMode === 'dongle'
        ? '<p class="rcp-loading">No phones are bonded yet — open a pairing window above and pair your handset.</p>'
        : '<p class="rcp-loading">No phones are bonded yet — pair your handset in Windows Settings → Bluetooth &amp; devices → Add device (a pairing window above makes this PC discoverable while you do).</p>';
    }
    var rows = [];
    for (var i = 0; i < bonded.length; i++) {
      var d = bonded[i];
      var addr = String(d.address || '');
      var rowBusy = !!busyPhones[addr] || busy;
      var isConn =
        d.connected ||
        (connected && device && device.address && addr.toLowerCase() === String(device.address).toLowerCase());
      rows.push(
        '<div class="rcp-phone-row">' +
          '<span class="rcp-phone-row__id">' +
          '<strong>' + esc(d.name || 'Unknown phone') + '</strong>' +
          '<small>' + esc(addr) + '</small>' +
          '</span>' +
          '<span class="rcp-pill ' + (isConn ? 'is-ok' : 'is-neutral') + '"><i></i>' + (isConn ? 'Connected' : 'Offline') + '</span>' +
          '<button type="button" class="rcp-button" data-act="' + (isConn ? 'ph-disconnect' : 'ph-connect') + '" data-addr="' + esc(addr) + '"' +
          (rowBusy ? ' disabled' : '') + '>' +
          (busyPhones[addr] ? (isConn ? 'Disconnecting…' : 'Reconnecting…') : isConn ? 'Disconnect' : 'Reconnect') +
          '</button>' +
          '<button type="button" class="rcp-button is-danger" data-act="ph-forget" data-addr="' + esc(addr) + '"' + (rowBusy ? ' disabled' : '') + '>Forget</button>' +
          '</div>' +
          (forgetArm === addr
            ? '<div class="rcp-confirm is-danger" style="margin: 0 15px 10px;">' +
              '<p><strong>Forget this phone?</strong> ' + esc(addr) + " won't be able to reconnect until you pair it again.</p>" +
              '<div class="rcp-actions">' +
              '<button type="button" class="rcp-button is-danger" data-act="ph-forget-confirm" data-addr="' + esc(addr) + '"' + (busy ? ' disabled' : '') + '>Forget</button>' +
              '<button type="button" class="rcp-button" data-act="ph-forget-cancel">Cancel</button>' +
              '</div>' +
              '</div>'
            : '')
      );
    }
    return '<div class="rcp-phone-list">' + rows.join('') + '</div>';
  }

  function render() {
    if (!root) return;
    var html =
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>BLUETOOTH PAIRING</small>' +
      '<h3>' +
      (confirmPrompt
        ? 'Confirm the pairing code'
        : secondsLeft > 0
          ? 'Pairing window open'
          : connected
            ? 'Phone connected'
            : 'Pair a phone') +
      '</h3>' +
      '</div>' +
      (secondsLeft > 0
        ? '<span class="rcp-pill is-warn"><i></i>' + esc(formatSeconds(secondsLeft)) + ' left</span>'
        : connected
          ? '<span class="rcp-pill is-ok"><i></i>Connected</span>'
          : '') +
      '</div>' +
      pairingCardBody() +
      (error ? '<p class="rcp-error">' + esc(error) + '</p>' : '') +
      '</section>' +
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>BONDED PHONES</small>' +
      '<h3>' +
      (known ? bonded.length + ' phone' + (bonded.length === 1 ? '' : 's') + ' bonded' : 'Loading…') +
      '</h3>' +
      '</div>' +
      '</div>' +
      bondedCardBody() +
      '<p class="rcp-footnote">Forget removes the bond — that phone must pair again before it can reconnect. ' +
      'Already-paired phones reconnect on their own.</p>' +
      '</section>';
    if (html === lastHtml) return;
    lastHtml = html;
    root.innerHTML = html;
  }

  // ---- wiring -------------------------------------------------------------

  function wire(el) {
    if (el.__aokiePhoneWired) return;
    el.__aokiePhoneWired = true;
    el.addEventListener('click', function (e) {
      var btn = e.target && e.target.closest ? e.target.closest('[data-act]') : null;
      if (!btn || btn.disabled) return;
      var act = btn.getAttribute('data-act');
      if (act === 'pair-start') startPairing();
      else if (act === 'pair-stop') stopPairing();
      else if (act === 'pair-confirm') resolvePairing(true);
      else if (act === 'pair-reject') resolvePairing(false);
      else if (act === 'ph-connect') phoneAct('phone.connect', btn.getAttribute('data-addr'));
      else if (act === 'ph-disconnect') phoneAct('phone.disconnect', btn.getAttribute('data-addr'));
      else if (act === 'ph-forget') {
        forgetArm = btn.getAttribute('data-addr');
        render();
      } else if (act === 'ph-forget-confirm') doForget(btn.getAttribute('data-addr'));
      else if (act === 'ph-forget-cancel') {
        forgetArm = null;
        render();
      }
    });
  }

  TABS.register('phone', {
    mount: function (el) {
      root = el;
      lastHtml = '';
      wire(el);
      render();
      refreshAll();
      if (idleTimer == null) {
        idleTimer = window.setInterval(function () {
          if (!document.hidden && root) refreshAll();
        }, IDLE_POLL_MS);
      }
      ensureTimers();
    },
    unmount: function () {
      root = null;
      if (idleTimer != null) {
        window.clearInterval(idleTimer);
        idleTimer = null;
      }
      if (windowTimer != null) {
        window.clearInterval(windowTimer);
        windowTimer = null;
      }
    },
    onEvent: function (evt) {
      var name = (evt && evt.name) || '';
      // A phone connecting/dropping/pairing updates the readout IMMEDIATELY
      // instead of waiting out the idle poll.
      if (name.indexOf('aokie.phone.') === 0 || name.indexOf('aokie.dongle.') === 0) {
        refreshAll();
      }
    },
  });
})();
