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
 *
 * Setup mode (the tab as the pairing step of OAIY's setup wizard, app.js):
 * the step is done when a BONDED phone is connected and no pairing window or
 * numeric comparison is open, by the same rule: a connection that comes up
 * mid-pairing is not yet a paired phone.
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
  /** The dongle takes one phone at a time: the radio turns a second phone
   *  away while one is connected, pairing included. */
  var ONE_PHONE_NOTE =
    'The dongle takes one phone at a time: a new phone cannot pair while another is connected. To pair another, first turn Bluetooth off on the connected phone (a phone only disconnected here reconnects by itself).';

  function formatSeconds(total) {
    var m = Math.floor(total / 60);
    var s = total % 60;
    return m + ':' + (s < 10 ? '0' : '') + s;
  }

  /** The pairing window's time left, in the markup render() compares: each
   *  2 s poll changes only this, and it is written into the spans in place
   *  rather than the tab drawn anew (which dropped keyboard focus from Stop
   *  pairing and Codes match, and a click pressed as the poll landed). */
  var COUNTDOWN = '\u2063countdown\u2063';
  var COUNTDOWN_HTML = '<span data-countdown>' + COUNTDOWN + '</span>';

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
  var radioError = null;
  var statusError = null;
  var bondedError = null;
  var busyPhones = {};
  var forgetArm = null; // address with the inline Forget confirm open
  // Disconnect with a call on the line asks first: { address, text } of the
  // inline confirm open, and the address whose call check is out.
  var disconnectArm = null;
  var disconnectChecking = null;
  // The phone line is not running ({ consent, reason } from phone.status):
  // nothing here can pair, reconnect or forget a phone until it does.
  var lineDown = null;
  var baselineBonds = 0;
  var confirmed = false; // operator accepted THIS window's comparison
  var confirmedAddress = null; // ...for this phone
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
      bondedError = null;
      var raw = (data && data.devices) || [];
      bonded = [];
      for (var i = 0; i < raw.length; i++) {
        var d = raw[i];
        if (typeof d === 'string') bonded.push({ address: d, name: null, connected: false });
        else if (d && d.address) bonded.push({ address: d.address, name: d.name || null, connected: !!d.connected });
      }
      return bonded;
    }, function (e) {
      bondedError = errMsg(e);
      throw e;
    });
  }

  function pollStatus() {
    return HOST.command('phone.status').then(function (data) {
      statusError = null;
      var d = data || {};
      secondsLeft =
        d.pairingOpen && typeof d.pairingSecondsRemaining === 'number'
          ? d.pairingSecondsRemaining
          : 0;
      // (no phone line: said as such, not as the radio's last Bluetooth error)
      lineDown = U.lineDownFromPhone(d);
      radioError = lineDown ? null : d.error || null;
      // Connected is what the plugin says: an old error kept in `error`
      // (shown below) does not make a connected phone read "Pair a phone".
      connected = !!d.connected && !d.pairingConfirm;
      device = d.device || null;
      // PAIR-001: surface (or clear) the numeric-comparison prompt. The
      // plugin reports it only while the radio actually holds the SSP reply.
      var pc = d.pairingConfirm;
      confirmPrompt =
        pc && pc.address && typeof pc.numericValue === 'number'
          ? { address: pc.address, numericValue: pc.numericValue }
          : null;
      return { secs: secondsLeft, connected: connected };
    }, function (e) {
      statusError = errMsg(e);
      throw e;
    });
  }

  // One refresh at a time: a slow radio answers late, and polls that stack
  // up behind it queue ahead of the commands that matter (Answer, Hang up).
  var refreshing = null;

  function refreshAll() {
    if (refreshing) return refreshing;
    // Every read is waited for, each failing on its own: a status read that
    // failed first once drew the paired list before it had been read ("No
    // phone is paired yet" over paired phones). A failed read is shown by
    // the card it is for (statusError, bondedError); `error` is an action's,
    // and stays until the next action.
    var settledRead = function (p) {
      return p.then(
        function () {
          return true;
        },
        function () {
          return false;
        }
      );
    };
    refreshing = Promise.all([settledRead(pollStatus()), settledRead(loadBonded()), settledRead(loadTransport())]).then(
      function () {
        known = true;
        afterPoll();
      }
    ).then(
      function () {
        refreshing = null;
      },
      // (as a finally: a render that threw above must not leave every later refresh waiting on this one)
      function (e) {
        refreshing = null;
        throw e;
      }
    );
    return refreshing;
  }

  // Best-effort transport fetch: a settings failure keeps the last-known
  // mode — it must never break the phone readouts. Also feeds the shared
  // transport truth in app.js (Dongle-tab visibility).
  function loadTransport() {
    return HOST.command('settings.get').then(
      function (data) {
        var bag = (data && data.settings) || {};
        if (TABS.transport && TABS.transport.update) TABS.transport.update(bag);
        var mode = TABS.transport && TABS.transport.mode ? TABS.transport.mode() : 'dongle';
        // (`auto` pairs with the dongle whenever one is plugged in)
        transportMode = mode === 'native' ? 'native' : 'dongle';
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

  var ticking = false;

  function windowTick() {
    if (ticking) return; // the last tick is still waiting on the radio
    ticking = true;
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
        // The phone whose code was accepted, connected: not just any phone
        // (another bonded phone reconnecting read as this one paired).
        var success = bonded.length > baselineBonds || (confirmed && confirmedPhoneConnected());
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
                'Phone paired. The Overview shows when the receptionist is ready for calls.'
              );
              return refreshAll();
            });
        }
        ensureTimers();
        render();
        return null;
      })
      .catch(function () {
        // The idle poll recovers, but stale connection or pairing prompts
        // must not stay actionable while the current status is unavailable.
        render();
      })
      .then(function () {
        ticking = false;
      });
  }

  /** Whether the phone this window's code was accepted for is the one now
   *  connected (by the status's device address, or the roster's flag). */
  function confirmedPhoneConnected() {
    if (!connected || !confirmedAddress) return false;
    var want = String(confirmedAddress).toLowerCase();
    if (device && device.address) return String(device.address).toLowerCase() === want;
    for (var i = 0; i < bonded.length; i++) {
      if (String(bonded[i].address || '').toLowerCase() === want) return !!bonded[i].connected;
    }
    return false;
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
            HOST.toast('info', 'No pairing window opened: this Aokie runs without a radio (a test build).');
            return;
          }
          secondsLeft = typeof d.windowSeconds === 'number' ? d.windowSeconds : PAIRING_WINDOW_SECONDS;
          prevWindowOpen = true;
          HOST.toast(
            'success',
            transportMode === 'dongle'
              ? 'Pairing open for 5 minutes. On your phone: Bluetooth › Pair new device › “Aokie AI Assistant”.'
              : 'This PC can be found for 5 minutes. Finish the pairing in Windows Settings › Bluetooth & devices › Add device.'
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
          if (accept) {
            confirmed = true;
            confirmedAddress = address;
          }
          confirmPrompt = null;
          if (!accept) {
            HOST.toast('info', 'Pairing refused: the phone was not paired. Pairing stays open if you want to try again.');
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
              ? 'Reconnecting the phone (' + address + '): it shows as connected once it is back.'
              : 'Disconnecting the phone (' + address + '): it may take a moment.'
          );
          return refreshAll();
        },
        function (e) {
          HOST.toast(
            'error',
            (command === 'phone.connect' ? 'The phone could not be reconnected: ' : 'The phone could not be disconnected: ') + errMsg(e)
          );
        }
      )
      .then(function () {
        delete busyPhones[address];
        render();
      });
  }

  /** Disconnect: with a call on the line, disconnecting takes it off the
   *  receptionist, so the plugin is asked first (the Phone tab does not
   *  follow calls) and the row asks before going on. */
  function disconnect(address) {
    if (!address || busyPhones[address] || disconnectChecking) return;
    disconnectChecking = address;
    render();
    HOST.command('call.current')
      .then(
        function (data) {
          var call = data && data.call;
          if (!call || !call.state || call.state === 'ended') return '';
          var who = call.from || 'the caller';
          return call.state === 'ringing' && call.direction !== 'outbound'
            ? 'A call from ' + who + ' is ringing: once the phone is disconnected, the receptionist cannot answer it.'
            : 'This ends the call with ' + who + '.';
        },
        function () {
          return 'Aokie cannot tell whether a call is on the line: if one is, this ends it.';
        }
      )
      .then(function (text) {
        disconnectChecking = null;
        if (!text) {
          phoneAct('phone.disconnect', address);
          return;
        }
        disconnectArm = { address: address, text: text };
        render();
        focusIn('[data-act="ph-disconnect-cancel"]');
      });
  }

  /** Focus `sel` in the tab once it is drawn again (an inline confirm opened or closed). */
  function focusIn(sel) {
    var el = root && root.querySelector ? root.querySelector(sel) : null;
    if (el && typeof el.focus === 'function') el.focus();
  }

  // ---- setup mode -----------------------------------------------------------

  /** The bonded phone that is connected now, or null. */
  function connectedBond() {
    if (!connected) return null;
    var live = device && device.address ? String(device.address).toLowerCase() : '';
    for (var i = 0; i < bonded.length; i++) {
      var d = bonded[i];
      if (d.connected || (live && String(d.address || '').toLowerCase() === live)) return d;
    }
    return null;
  }

  /** Setup mode: tell the wizard once a paired phone is connected. */
  function reportSetup() {
    var S = TABS.setup;
    if (!S || !S.active() || !known) return;
    var bond = !statusError && !bondedError && secondsLeft === 0 && !confirmPrompt ? connectedBond() : null;
    if (bond) S.done('Paired with ' + (bond.name || (device && device.name) || bond.address));
    else S.unsatisfied();
  }

  // ---- rendering ----------------------------------------------------------

  function pairingCardBody() {
    if (!known) return U.loadingHtml;
    if (statusError) return '<p class="rcp-hint">The phone status could not be verified. Retrying automatically…</p>';

    if (confirmPrompt) {
      return (
        '<div class="rcp-card__body">' +
        '<p class="rcp-step-meta">Phone <strong>' + esc(confirmPrompt.address) + '</strong> wants to pair. ' +
        'Confirm only if your phone shows this same code:</p>' +
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
          ? 'Phones nearby can see “Aokie AI Assistant” for <strong>' + COUNTDOWN_HTML + '</strong> more. ' +
            'On your phone: Bluetooth › Pair new device › Aokie AI Assistant, then confirm the matching code here.'
          : 'This PC can be found by nearby phones for <strong>' + COUNTDOWN_HTML + '</strong> more. ' +
            'Finish the pairing in Windows Settings › Bluetooth &amp; devices › Add device; Windows shows the code to confirm.';
      return (
        '<div class="rcp-card__body">' +
        '<p class="rcp-step-meta">' + windowCopy + '</p>' +
        (transportMode === 'dongle' && connected
          ? '<p class="rcp-hint is-warn">' + esc(ONE_PHONE_NOTE) + '</p>'
          : '') +
        '<div class="rcp-actions">' +
        '<button type="button" class="rcp-button" data-act="pair-stop"' + (busy ? ' disabled' : '') + '>Stop pairing</button>' +
        '</div>' +
        '</div>'
      );
    }

    // No phone line: no pairing window can open, so none is offered; what
    // the line waits on is said once, with where it is put right.
    if (lineDown) {
      var builtIn = transportMode !== 'dongle';
      return (
        '<div class="rcp-card__body">' +
        '<p class="rcp-step-meta">' +
        (lineDown.consent
          ? 'The phone line is paused until consent is given again' +
            (lineDown.reason ? ': ' + esc(U.sentence(lineDown.reason).replace(/\.+$/, '')) : '') +
            '. Phones pair and reconnect once it is.'
          : 'Aokie’s phone line is not running' +
            (lineDown.reason ? ': ' + esc(U.plainTimes(lineDown.reason).replace(/\.+$/, '')) : '') +
            '. ' + (builtIn ? 'A phone can pair once it is.' : 'A phone can pair once the dongle is set up and Aokie’s radio is running.')) +
        '</p>' +
        (lineDown.consent
          ? '<div class="rcp-actions"><button type="button" class="rcp-button is-primary" data-tabgo="consent">Review consent</button></div>'
          : builtIn
            ? ''
            : '<div class="rcp-actions"><button type="button" class="rcp-button is-primary" data-tabgo="dongle">Open the Dongle tab</button></div>') +
        '</div>'
      );
    }

    if (connected) {
      var name = device && (device.name || device.address);
      return (
        '<div class="rcp-card__body">' +
        '<p class="rcp-status-ok">' + ICONS.check + '<span>Phone connected' +
        (name ? ': <strong>' + esc(name) + '</strong>' : '') +
        (device && device.name && device.address ? ' <small>' + esc(device.address) + '</small>' : '') +
        '</span></p>' +
        (transportMode === 'dongle' ? '<p class="rcp-hint">' + esc(ONE_PHONE_NOTE) + '</p>' : '') +
        '<div class="rcp-actions">' +
        '<button type="button" class="rcp-button" data-act="pair-start"' + (busy ? ' disabled' : '') + '>' +
        (busy ? 'Opening…' : 'Pair another phone') + '</button>' +
        '</div>' +
        '</div>'
      );
    }

    var idleCopy =
      transportMode === 'dongle'
        ? 'A new phone can find the dongle only while pairing is open. Phones already paired reconnect by themselves.'
        : 'Pair the phone in Windows Settings › Bluetooth &amp; devices › Add device. Opening pairing here lets the phone find this PC while you do. ' +
          'Phones already paired reconnect by themselves.';
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
    // (a list the radio could not give for not running: no failure of its own)
    if (bondedError && U.radioDownReason(bondedError) !== null) {
      return '<p class="rcp-empty">The paired phones show here once the phone line is running.</p>';
    }
    if (bondedError) return U.problemHtml(bondedError);
    if (!known) return U.loadingHtml;
    if (bonded.length === 0) {
      if (lineDown) return '<p class="rcp-empty">No phone is paired yet.</p>';
      return transportMode === 'dongle'
        ? '<p class="rcp-empty">No phone is paired yet. Press Pair a phone, then pair your phone with “Aokie AI Assistant”.</p>'
        : '<p class="rcp-empty">No phone is paired yet. Pair your phone in Windows Settings › Bluetooth &amp; devices › Add device.</p>';
    }
    var rows = [];
    for (var i = 0; i < bonded.length; i++) {
      var d = bonded[i];
      var addr = String(d.address || '');
      var rowBusy = !!busyPhones[addr] || busy || disconnectChecking === addr;
      // (the status's own device only while its read is current)
      var isConn =
        d.connected ||
        (!statusError && connected && device && device.address && addr.toLowerCase() === String(device.address).toLowerCase());
      var armed = !!(disconnectArm && disconnectArm.address === addr && isConn);
      rows.push(
        '<div class="rcp-phone-row">' +
          '<span class="rcp-phone-row__icon' + (isConn ? ' is-ok' : '') + '">' + ICONS.smartphone + '</span>' +
          '<span class="rcp-phone-row__id">' +
          '<strong>' + esc(d.name || 'Unnamed phone') + '</strong>' +
          '<small>' + esc(addr) + '</small>' +
          '</span>' +
          '<span class="rcp-phone-row__actions">' +
          '<span class="rcp-pill ' + (isConn ? 'is-ok' : 'is-neutral') + '"><i></i>' + (isConn ? 'Connected' : 'Offline') + '</span>' +
          // (no phone line: none of these can be done until it runs)
          (lineDown
            ? ''
            : '<button type="button" class="rcp-button is-small" data-act="' + (isConn ? 'ph-disconnect' : 'ph-connect') + '" data-addr="' + esc(addr) + '"' +
              (rowBusy || armed ? ' disabled' : '') + '>' +
              (busyPhones[addr] ? (isConn ? 'Disconnecting…' : 'Reconnecting…') : isConn ? 'Disconnect' : 'Reconnect') +
              '</button>' +
              '<button type="button" class="rcp-button is-small is-danger" data-act="ph-forget" data-addr="' + esc(addr) + '"' + (rowBusy || forgetArm === addr ? ' disabled' : '') + '>Forget</button>') +
          '</span>' +
          (armed
            ? '<div class="rcp-confirm is-danger" role="group" aria-label="Disconnect while a call is on the line">' +
              '<p><strong>Disconnect ' + esc(d.name || 'this phone') + '?</strong> ' + esc(disconnectArm.text) + '</p>' +
              '<div class="rcp-actions">' +
              '<button type="button" class="rcp-button is-danger" data-act="ph-disconnect-confirm" data-addr="' + esc(addr) + '"' + (rowBusy ? ' disabled' : '') + '>Disconnect anyway</button>' +
              '<button type="button" class="rcp-button" data-act="ph-disconnect-cancel" data-addr="' + esc(addr) + '">Keep it connected</button>' +
              '</div>' +
              '</div>'
            : '') +
          (forgetArm === addr
            ? '<div class="rcp-confirm is-danger" role="group" aria-label="Forget this phone">' +
              '<p><strong>Forget ' + esc(d.name || 'this phone') + '?</strong> It cannot reconnect until you pair it again.</p>' +
              '<div class="rcp-actions">' +
              '<button type="button" class="rcp-button is-danger" data-act="ph-forget-confirm" data-addr="' + esc(addr) + '"' + (busy ? ' disabled' : '') + '>Forget phone</button>' +
              '<button type="button" class="rcp-button" data-act="ph-forget-cancel" data-addr="' + esc(addr) + '">Cancel</button>' +
              '</div>' +
              '</div>'
            : '') +
          '</div>'
      );
    }
    return (
      '<div class="rcp-phone-list">' + rows.join('') + '</div>' +
      (lineDown
        ? '<p class="rcp-empty">' +
          (lineDown.consent ? 'Phones reconnect by themselves once consent is given again.' : 'Phones reconnect by themselves once the phone line is running.') +
          '</p>'
        : '')
    );
  }

  function render() {
    if (!root) return;
    reportSetup();
    // The pairing card's own errors: an action that failed, and the radio's
    // last Bluetooth error (the status's own read failing is said above).
    var problems =
      (error ? '<p class="rcp-error">' + esc(error) + '</p>' : '') +
      (statusError ? U.problemHtml(statusError) : '') +
      (radioError && !statusError ? '<p class="rcp-error">Last Bluetooth error: ' + esc(radioError) + '</p>' : '');
    var html =
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>Bluetooth pairing</small>' +
      '<h3>' +
      (statusError
        ? 'Phone status unavailable'
        : lineDown
        ? lineDown.consent ? 'Paused for consent' : 'No phone line yet'
        : confirmPrompt
        ? 'Confirm the pairing code'
        : secondsLeft > 0
          ? 'Pairing window open'
          : connected
            ? 'Phone connected'
            : 'Pair a phone') +
      '</h3>' +
      '</div>' +
      (statusError
        ? '<span class="rcp-pill is-warn"><i></i>Unknown</span>'
        : secondsLeft > 0
        ? '<span class="rcp-pill is-warn"><i></i>' + COUNTDOWN_HTML + ' left</span>'
        : connected
          ? '<span class="rcp-pill is-ok"><i></i>Connected</span>'
          : '') +
      '</div>' +
      pairingCardBody() +
      problems +
      '</section>' +
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>Paired phones</small>' +
      '<h3>' +
      (bondedError
        ? U.radioDownReason(bondedError) !== null ? 'Not known yet' : 'Paired phones unavailable'
        : !known
          ? 'Checking paired phones…'
          : bonded.length === 0
            ? 'No phones paired'
            : bonded.length + ' phone' + (bonded.length === 1 ? '' : 's') + ' paired') +
      '</h3>' +
      '</div>' +
      '</div>' +
      bondedCardBody() +
      '<p class="rcp-footnote">Forget unpairs a phone: it must be paired again before it can reconnect. ' +
      'Phones still paired reconnect by themselves.</p>' +
      '</section>';
    var left = esc(formatSeconds(secondsLeft));
    if (html !== lastHtml) {
      lastHtml = html;
      // (drawn anew: the control in use keeps focus, U.setHtml)
      U.setHtml(root, html.split(COUNTDOWN).join(left));
      return;
    }
    var spans = root.querySelectorAll ? root.querySelectorAll('[data-countdown]') : [];
    for (var i = 0; i < spans.length; i++) {
      if (spans[i].textContent !== left) spans[i].textContent = left;
    }
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
      else if (act === 'ph-disconnect') disconnect(btn.getAttribute('data-addr'));
      else if (act === 'ph-disconnect-confirm') {
        disconnectArm = null;
        phoneAct('phone.disconnect', btn.getAttribute('data-addr'));
      } else if (act === 'ph-disconnect-cancel') {
        var kept = btn.getAttribute('data-addr');
        disconnectArm = null;
        render();
        focusIn('[data-act="ph-disconnect"][data-addr="' + String(kept || '').replace(/["\\]/g, '\\$&') + '"]');
      } else if (act === 'ph-forget') {
        forgetArm = btn.getAttribute('data-addr');
        render();
        focusIn('[data-act="ph-forget-cancel"]');
      } else if (act === 'ph-forget-confirm') doForget(btn.getAttribute('data-addr'));
      else if (act === 'ph-forget-cancel') {
        var unforgotten = btn.getAttribute('data-addr');
        forgetArm = null;
        render();
        focusIn('[data-act="ph-forget"][data-addr="' + String(unforgotten || '').replace(/["\\]/g, '\\$&') + '"]');
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
