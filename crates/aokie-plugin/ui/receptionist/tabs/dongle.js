/*
 * Dongle tab — guided WinUSB driver setup for the Aokie Bluetooth dongle.
 *
 * Faithful port of FormLogic Desktop's compiled DongleSetupWizard
 * (desktop/src/aokie/DongleSetupWizard.tsx; AOK-DRIVER-001 backend): a
 * three-step wizard — pick the dongle, install the driver (elevated helper
 * behind a UAC prompt), verify the rebind — plus the advanced "restore the
 * Windows driver" escape hatch. All device work happens in the plugin/host
 * (`dongle.list` / `dongle.installDriver` / `dongle.restoreDriver`); this
 * tab only sequences it and narrates what to expect.
 *
 * A second card, "Reset the dongle", runs `dongle.reset`: the plugin's
 * software recovery (reopen the USB connection, reset the controller, page
 * the phone back), refused by the plugin while a call is up.
 *
 * Sandbox deltas: the wizard renders as a whole tab (never collapsed), the
 * restore confirm is INLINE (no host dialog), and the done step points at
 * the Phone setup TAB (pairing no longer lives "below" on the same card).
 *
 * Setup mode (the tab as a step of OAIY's setup wizard, app.js): the step
 * is done when Aokie's radio is up on the dongle (`dongle.diagnostics`
 * radio.initialized). A driver installed after Aokie started is not picked
 * up by the running radio, so the step offers to start Aokie again
 * (PluginHost.restartPlugin). On the built-in Bluetooth transport there is
 * no dongle to set up and the step says so. The reset card is not shown.
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

  // ---- state --------------------------------------------------------------

  var root = null;
  var step = 'select'; // 'select' | 'install' | 'verify' | 'done'
  var list = null; // last dongle.list payload (null = not yet loaded)
  var loading = false;
  var selectedKey = null; // "vid:pid" of the picked device
  var busy = false;
  var error = null;
  var restoreArm = null; // "vid:pid" with the inline restore confirm open
  var verifyTimer = null;
  var verifyAttempts = 0;
  // The device a verify poll is checking. Kept OUTSIDE the live list because
  // the target legitimately disappears from `connected` mid-verify (the
  // rebind re-enumerates it) — "Check again" and a remount mid-verify must
  // still know which device they're waiting on.
  var verifyTarget = null;
  // "Reset the dongle" (dongle.reset): its own busy flag and last outcome,
  // separate from the driver wizard's.
  var resetting = false;
  var resetError = null;
  var resetNote = null;
  // Setup mode only (see the header).
  var setupRadio = undefined; // dongle.diagnostics `radio`: undefined unchecked, null unavailable
  var setupTransportKnown = false; // settings.get answered, so the transport mode is known
  var radioStarting = false; // PluginHost.restartPlugin + waiting for the radio
  var radioStartError = null;
  var radioStartTimer = null;

  function setupActive() {
    return !!(TABS.setup && TABS.setup.active());
  }

  /** The built-in Windows Bluetooth transport: no dongle is involved. */
  function builtInTransport() {
    return setupTransportKnown && !!TABS.transport && TABS.transport.mode() !== 'dongle';
  }

  function radioUp() {
    return !!setupRadio && setupRadio.initialized === true;
  }

  function key(d) {
    return d.vid + ':' + d.pid;
  }

  function dongleLabel(d) {
    return d.description || d.hardwareId || 'USB Bluetooth adapter';
  }

  function idLabel(d) {
    if (d.vidHex && d.pidHex) return d.vidHex + ':' + d.pidHex;
    var vid = Number(d.vid).toString(16);
    var pid = Number(d.pid).toString(16);
    while (vid.length < 4) vid = '0' + vid;
    while (pid.length < 4) pid = '0' + pid;
    return vid + ':' + pid;
  }

  function connectedDevices() {
    return (list && list.connected) || [];
  }

  function selectedDevice() {
    if (!selectedKey) return null;
    var conn = connectedDevices();
    for (var i = 0; i < conn.length; i++) {
      if (key(conn[i]) === selectedKey) return conn[i];
    }
    return null;
  }

  function refresh() {
    loading = true;
    render();
    return HOST.command('dongle.list').then(
      function (data) {
        list = data || {};
        error = null;
        // Keep the selection pinned to the same physical id across refreshes.
        if (selectedKey && !selectedDevice()) selectedKey = null;
        loading = false;
        render();
        return list;
      },
      function (e) {
        error = errMsg(e);
        loading = false;
        render();
        return null;
      }
    );
  }

  /** Post-install verification: poll the list until the target shows up as
   *  WinUSB-bound (the rebind + re-enumeration can lag the helper's exit). */
  function verifySelected(target) {
    step = 'verify';
    error = null;
    verifyAttempts = 0;
    verifyTarget = target;
    render();
    var targetKey = key(target);
    var poll = function () {
      refresh().then(function (data) {
        if (!root) return; // tab left mid-verify (mount resumes the poll)
        var now = null;
        var conn = (data && data.connected) || [];
        for (var i = 0; i < conn.length; i++) {
          if (key(conn[i]) === targetKey) now = conn[i];
        }
        if (now && now.driverBound) {
          step = 'done';
          selectedKey = targetKey;
          verifyTarget = null;
          HOST.toast(
            'success',
            'Dongle driver installed — ' + dongleLabel(target) + ' is now bound to WinUSB and ready for Aokie.'
          );
          render();
          if (setupActive()) checkRadio();
          return;
        }
        verifyAttempts += 1;
        if (verifyAttempts < 6) {
          verifyTimer = window.setTimeout(poll, 2000);
        } else {
          error =
            'The driver install finished but the dongle has not re-appeared as WinUSB-bound yet. ' +
            'Unplug and replug the dongle, then press "Check again".';
          render();
        }
      });
    };
    poll();
  }

  function install() {
    var sel = selectedDevice();
    if (!sel) return;
    busy = true;
    error = null;
    render();
    HOST.command('dongle.installDriver', { vid: sel.vid, pid: sel.pid })
      .then(
        function () {
          busy = false;
          verifySelected(sel);
        },
        function (e) {
          // The declined-UAC case comes back as a typed, human error from the host.
          busy = false;
          error = errMsg(e);
          step = 'install';
          render();
        }
      );
  }

  function restore(d) {
    busy = true;
    error = null;
    restoreArm = null;
    render();
    HOST.command('dongle.restoreDriver', { vid: d.vid, pid: d.pid })
      .then(
        function () {
          HOST.toast('success', 'Windows driver restored — the dongle is back on the standard Windows Bluetooth driver.');
          return refresh();
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

  /** Reset the dongle in software (dongle.reset): the plugin closes and
   *  reopens the USB connection, resets the Bluetooth controller and pages
   *  the phone back, then answers within about 15 seconds. It refuses
   *  during a call, with a message that says so. */
  function resetDongle() {
    if (resetting) return;
    resetting = true;
    resetError = null;
    resetNote = null;
    render();
    HOST.command('dongle.reset', {})
      .then(
        function (data) {
          var back = !!(data && data.phoneReconnected);
          resetNote = {
            ok: back,
            text: back
              ? 'The dongle was reset and the phone is connected again.'
              : 'The dongle was reset. The phone has not reconnected yet; it usually does within a minute (see the Phone setup tab).',
          };
          HOST.toast(back ? 'success' : 'info', back ? 'Dongle reset — the phone is connected again.' : 'Dongle reset — waiting for the phone to reconnect.');
        },
        function (e) {
          resetError = errMsg(e);
        }
      )
      .then(function () {
        resetting = false;
        render();
      });
  }

  // ---- setup mode -----------------------------------------------------------

  /** Setup mode: tell the wizard once the radio is up on a dongle, or that
   *  the built-in transport needs none. */
  function reportSetup() {
    var S = TABS.setup;
    if (!S || !S.active()) return;
    if (builtInTransport()) {
      S.done('Built-in Bluetooth: no dongle needed');
      return;
    }
    if (radioUp()) {
      S.done('Dongle ready' + (setupRadio.localAddress ? ' (' + setupRadio.localAddress + ')' : ''));
      return;
    }
    S.unsatisfied();
  }

  /** Read the radio's state (dongle.diagnostics, read-only). A rejection
   *  means no radio: startup failed or Aokie is restarting. */
  function checkRadio() {
    return HOST.command('dongle.diagnostics').then(
      function (data) {
        setupRadio = (data && data.radio) || null;
      },
      function () {
        setupRadio = null;
      }
    ).then(function () {
      reportSetup();
      render();
    });
  }

  /** Setup mode on mount: which transport, and is the radio up already? */
  function setupCheck() {
    var transportP = HOST.command('settings.get').then(
      function (data) {
        var bag = (data && data.settings) || {};
        if (TABS.transport && TABS.transport.update) TABS.transport.update(bag);
        setupTransportKnown = true;
      },
      function () {
        /* unknown: the dongle view, as in normal mode */
      }
    );
    return transportP.then(checkRadio);
  }

  /** Start Aokie again so its radio opens a dongle whose driver was
   *  installed after Aokie started, then wait for the radio (~30 s). */
  function startRadio() {
    if (radioStarting) return;
    if (typeof HOST.restartPlugin !== 'function') {
      radioStartError =
        'This OAIY Desktop cannot restart Aokie from here. Restart the Aokie plugin from the Plugins page, then come back to this step.';
      render();
      return;
    }
    radioStarting = true;
    radioStartError = null;
    render();
    var restarted;
    try {
      restarted = Promise.resolve(HOST.restartPlugin());
    } catch (e) {
      restarted = Promise.reject(e);
    }
    restarted
      .then(function () {
        return new Promise(function (resolve) {
          var attempts = 0;
          var poll = function () {
            radioStartTimer = null;
            if (!root) {
              resolve(false);
              return;
            }
            HOST.command('dongle.diagnostics')
              .then(
                function (data) {
                  setupRadio = (data && data.radio) || null;
                },
                function () {
                  setupRadio = null; // still starting
                }
              )
              .then(function () {
                if (radioUp()) {
                  resolve(true);
                  return;
                }
                attempts += 1;
                if (attempts >= 15) resolve(false);
                else radioStartTimer = window.setTimeout(poll, 2000);
              });
          };
          poll();
        });
      })
      .then(
        function (up) {
          radioStarting = false;
          if (!up) {
            radioStartError =
              'Aokie started again, but the dongle has not come up yet. Unplug the dongle, plug it back in, then try again.';
          }
          reportSetup();
          render();
        },
        function (e) {
          radioStarting = false;
          radioStartError = 'Aokie could not be started again: ' + errMsg(e);
          render();
        }
      );
  }

  /** Report the inner step to the wizard as progress. */
  function reportProgress() {
    var S = TABS.setup;
    if (!S || !S.active() || builtInTransport()) return;
    if (radioUp()) S.progress(1, 'Dongle ready');
    else if (step === 'select') S.progress(0, 'Choose your Bluetooth dongle');
    else if (step === 'install') S.progress(1 / 3, 'Install the WinUSB driver');
    else if (step === 'verify') S.progress(2 / 3, 'Verifying the driver');
    else S.progress(2 / 3, 'Start Aokie with the dongle');
  }

  /** Setup mode: the radio's state above the wizard's steps. */
  function setupStatusHtml() {
    if (!setupActive() || setupRadio === undefined) return '';
    if (radioUp()) {
      return (
        '<p class="rcp-status-ok">' + ICONS.check + ' Aokie’s radio is up on the dongle' +
        (setupRadio.localAddress ? ' (' + esc(setupRadio.localAddress) + ')' : '') +
        '. Press Next to pair your phone.</p>'
      );
    }
    var bound = false;
    var conn = connectedDevices();
    for (var i = 0; i < conn.length; i++) {
      if (conn[i].driverBound) bound = true;
    }
    if (!bound && !radioStarting && !radioStartError) return '';
    return (
      '<div class="rcp-callout is-warn">' + ICONS.alert +
      '<span>The dongle has its driver, but Aokie’s radio has not opened it: Aokie started before the driver was there. ' +
      'Start Aokie again to open the dongle. It takes a few seconds.</span></div>' +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button is-primary" data-act="dg-setup-start"' + (radioStarting ? ' disabled' : '') + '>' +
      (radioStarting ? 'Starting Aokie — opening the dongle…' : 'Start Aokie with this dongle') +
      '</button>' +
      '</div>' +
      (radioStartError && !radioStarting ? '<div class="rcp-error" style="padding: 10px 0 0;">' + esc(radioStartError) + '</div>' : '')
    );
  }

  /** Setup mode on the built-in transport: nothing to set up here. */
  function builtInHtml() {
    return (
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>Dongle setup</small>' +
      '<h3>No dongle needed</h3>' +
      '</div>' +
      '</div>' +
      '<div class="rcp-card__body">' +
      '<p class="rcp-status-ok">' + ICONS.check + ' Aokie uses this computer’s built-in Bluetooth, so there is no USB dongle to set up. ' +
      'Press Next to pair your phone.</p>' +
      '</div>' +
      '</section>'
    );
  }

  // ---- rendering ----------------------------------------------------------

  function stepHeading() {
    var stepIndex = step === 'select' ? 1 : step === 'install' ? 2 : 3;
    var title =
      step === 'select'
        ? 'Choose your Bluetooth dongle'
        : step === 'install'
          ? 'Install the WinUSB driver'
          : step === 'verify'
            ? 'Verifying the driver'
            : 'Dongle ready';
    return (
      '<p class="rcp-step-meta"><strong>Step ' + stepIndex + ' of 3 — ' + esc(title) + '</strong></p>'
    );
  }

  function selectStepHtml() {
    var conn = connectedDevices();
    var html = [];
    if (list && list.liveEnumeration === false) {
      html.push(
        '<div class="rcp-callout is-warn">' + ICONS.alert + '<span>Live USB scan unavailable' +
          (list.note ? ': ' + esc(list.note) : '') + '.</span></div>'
      );
    }
    if (list === null && loading) {
      html.push('<p class="rcp-loading" style="padding: 12px 0 0;">Scanning USB devices…</p>');
    } else if (conn.length === 0) {
      html.push(
        '<p class="rcp-step-meta">' +
          (loading
            ? 'Scanning USB devices…'
            : 'No supported dongle detected. Plug the USB Bluetooth dongle in, then rescan. ' +
              'Supported chipsets: Broadcom BCM20702 (certified), Realtek RTL8761/RTL8821CE and CSR8510 (beta).') +
          '</p>'
      );
    } else {
      var rows = [];
      for (var i = 0; i < conn.length; i++) {
        var d = conn[i];
        var isSel = selectedKey === key(d);
        rows.push(
          '<label class="rcp-wizard-row' + (isSel ? ' is-selected' : '') + '">' +
            '<input type="radio" name="dongle-pick" data-pick="' + esc(key(d)) + '"' + (isSel ? ' checked' : '') + ' />' +
            '<span class="rcp-wizard-row__name">' + esc(dongleLabel(d)) +
            '<small> · USB ' + esc(idLabel(d)) + '</small></span>' +
            '<span class="rcp-wizard-row__badges">' +
            (d.matchesCatalog
              ? '<span class="rcp-badge is-ok">supported</span>'
              : '<span class="rcp-badge is-neutral">unknown chipset</span>') +
            (d.driverBound
              ? '<span class="rcp-badge is-ok">WinUSB installed</span>'
              : '<span class="rcp-badge is-pending">driver required</span>') +
            '</span>' +
            '</label>'
        );
      }
      html.push('<div class="rcp-wizard-list">' + rows.join('') + '</div>');
    }

    var sel = selectedDevice();
    var actions = [
      '<button type="button" class="rcp-button" data-act="dg-rescan"' + (loading ? ' disabled' : '') + '>' +
        (loading ? 'Scanning…' : 'Rescan') + '</button>',
    ];
    if (sel && !sel.driverBound) {
      actions.push('<button type="button" class="rcp-button is-primary" data-act="dg-continue">Continue</button>');
    }
    if (sel && sel.driverBound) {
      actions.push(
        '<span class="rcp-status-ok">' + ICONS.check + " This dongle already has the WinUSB driver — it's ready for Aokie.</span>"
      );
      actions.push(
        '<button type="button" class="rcp-button is-warn" data-act="dg-restore-arm" data-pick="' + esc(key(sel)) + '"' +
          (busy ? ' disabled' : '') + '>Restore Windows driver</button>'
      );
    }
    html.push('<div class="rcp-actions">' + actions.join('') + '</div>');

    if (sel && restoreArm === key(sel)) {
      html.push(
        '<div class="rcp-confirm is-danger">' +
          '<p><strong>Restore the Windows Bluetooth driver?</strong> ' + esc(dongleLabel(sel)) +
          ' will be handed back to the standard Windows driver (BTHUSB). Aokie will no longer be able to use it ' +
          'until you install the WinUSB driver again. Windows will show an elevation prompt.</p>' +
          '<div class="rcp-actions">' +
          '<button type="button" class="rcp-button is-danger" data-act="dg-restore" data-pick="' + esc(key(sel)) + '"' + (busy ? ' disabled' : '') + '>Restore driver</button>' +
          '<button type="button" class="rcp-button" data-act="dg-restore-cancel">Cancel</button>' +
          '</div>' +
          '</div>'
      );
    }
    return html.join('');
  }

  function installStepHtml() {
    var sel = selectedDevice();
    if (!sel) return '<p class="rcp-error">The selected dongle disappeared — rescan and pick it again.</p>';
    return (
      '<p class="rcp-step-meta">Installing for <strong>' + esc(dongleLabel(sel)) + '</strong> (USB ' + esc(idLabel(sel)) + ').' +
      (!sel.matchesCatalog
        ? " This chipset isn't in the supported catalog — the host will refuse the install unless the unknown-dongle override is set."
        : '') +
      '</p>' +
      '<p class="rcp-step-meta">Windows will show a <strong>User Account Control</strong> prompt — choose <strong>Yes</strong>. ' +
      'Only this exact device is rebound (the installer pins its hardware identity), and you can restore the standard ' +
      'Windows driver at any time.</p>' +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button" data-act="dg-back"' + (busy ? ' disabled' : '') + '>Back</button>' +
      '<button type="button" class="rcp-button is-primary" data-act="dg-install"' + (busy ? ' disabled' : '') + '>' +
      (busy ? 'Installing — watch for the Windows prompt…' : 'Install WinUSB driver') +
      '</button>' +
      '</div>'
    );
  }

  function verifyStepHtml() {
    return '<p class="rcp-step-meta">Checking that the dongle re-appeared as WinUSB-bound…</p>';
  }

  function doneStepHtml() {
    var sel = selectedDevice();
    return (
      '<p class="rcp-status-ok">' + ICONS.check + ' <strong>' + esc(sel ? dongleLabel(sel) : 'The dongle') +
      '</strong>&nbsp;is bound to WinUSB and ready.</p>' +
      // In setup mode the wizard's next step is the pairing; the radio's
      // state above says when to press Next.
      (setupActive()
        ? ''
        : '<p class="rcp-step-meta">Next: open the ' +
          '<button type="button" class="rcp-link-btn" data-tabgo="phone">Phone setup tab</button>' +
          ' and pair your handset (Bluetooth pairing → "Pair a phone").</p>') +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button" data-act="dg-again">Set up another dongle</button>' +
      '</div>'
    );
  }

  function render() {
    if (!root) return;
    reportProgress();
    if (setupActive() && builtInTransport()) {
      root.innerHTML = builtInHtml();
      return;
    }
    var body;
    if (step === 'select') body = selectStepHtml();
    else if (step === 'install') body = installStepHtml();
    else if (step === 'verify') body = verifyStepHtml();
    else body = doneStepHtml();

    root.innerHTML =
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>Dongle setup</small>' +
      '<h3>USB Bluetooth dongle driver</h3>' +
      '</div>' +
      '</div>' +
      '<div class="rcp-card__body">' +
      '<p class="rcp-inline-note">Guided setup for the USB Bluetooth dongle: pick the device, install the WinUSB ' +
      "driver (one Windows prompt), and verify it's ready.</p>" +
      setupStatusHtml() +
      stepHeading() +
      body +
      (error
        ? '<div class="rcp-error" style="padding: 10px 0 0;">' + esc(error) +
          (step === 'verify'
            ? ' <button type="button" class="rcp-button" data-act="dg-verify-again"' + (busy ? ' disabled' : '') + '>Check again</button>'
            : '') +
          '</div>'
        : '') +
      '</div>' +
      '<p class="rcp-footnote">All device work happens in the plugin and its elevated helper — this screen only ' +
      'sequences it. Restoring the Windows driver at any time hands the dongle back to BTHUSB.</p>' +
      '</section>' +
      // Recovery for a working receptionist, not part of setting one up.
      (setupActive() ? '' : resetCardHtml());
  }

  /** The "Reset the dongle" card: software recovery without unplugging. */
  function resetCardHtml() {
    return (
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>Recovery</small>' +
      '<h3>Reset the dongle</h3>' +
      '</div>' +
      '</div>' +
      '<div class="rcp-card__body">' +
      '<p class="rcp-inline-note">When calls or texts stop getting through and the dongle seems stuck, reset it ' +
      'here instead of unplugging it: Aokie closes and reopens its USB connection, resets the Bluetooth ' +
      'controller and reconnects the phone. The phone drops off for a few seconds. It cannot be done during a call.</p>' +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button" data-act="dg-reset"' + (resetting ? ' disabled' : '') + '>' +
      (resetting ? 'Resetting the dongle…' : 'Reset the dongle') +
      '</button>' +
      '</div>' +
      (resetNote && !resetting
        ? resetNote.ok
          ? '<p class="rcp-status-ok">' + ICONS.check + ' ' + esc(resetNote.text) + '</p>'
          : '<p class="rcp-step-meta">' + esc(resetNote.text) + '</p>'
        : '') +
      (resetError && !resetting ? '<div class="rcp-error" style="padding: 10px 0 0;">' + esc(resetError) + '</div>' : '') +
      '</div>' +
      '</section>'
    );
  }

  // ---- wiring -------------------------------------------------------------

  function wire(el) {
    if (el.__aokieDongleWired) return;
    el.__aokieDongleWired = true;
    el.addEventListener('change', function (e) {
      var t = e.target;
      if (!t || !t.getAttribute) return;
      var pick = t.getAttribute('data-pick');
      if (pick != null && t.type === 'radio') {
        selectedKey = pick;
        restoreArm = null;
        render();
      }
    });
    el.addEventListener('click', function (e) {
      var btn = e.target && e.target.closest ? e.target.closest('[data-act]') : null;
      if (!btn || btn.disabled) return;
      var act = btn.getAttribute('data-act');
      if (act === 'dg-rescan') refresh();
      else if (act === 'dg-continue') {
        step = 'install';
        error = null;
        render();
      } else if (act === 'dg-back') {
        step = 'select';
        render();
      } else if (act === 'dg-install') install();
      else if (act === 'dg-restore-arm') {
        restoreArm = btn.getAttribute('data-pick');
        render();
      } else if (act === 'dg-restore-cancel') {
        restoreArm = null;
        render();
      } else if (act === 'dg-restore') {
        var sel = selectedDevice();
        if (sel && key(sel) === btn.getAttribute('data-pick')) restore(sel);
      } else if (act === 'dg-verify-again') {
        // The verify target may be ABSENT from the live list mid-rebind —
        // retry against the captured target, not the (possibly null)
        // current selection.
        var target = verifyTarget || selectedDevice();
        if (target) verifySelected(target);
      } else if (act === 'dg-reset') {
        resetDongle();
      } else if (act === 'dg-setup-start') {
        startRadio();
      } else if (act === 'dg-again') {
        step = 'select';
        error = null;
        verifyTarget = null;
        refresh();
      }
    });
  }

  TABS.register('dongle', {
    mount: function (el) {
      root = el;
      wire(el);
      if (setupActive()) setupCheck();
      if (step === 'verify' && verifyTarget) {
        // The tab was left mid-verify (unmount cleared the poll timer) —
        // resume the verification instead of showing "Checking…" forever.
        verifySelected(verifyTarget);
        return;
      }
      render();
      refresh();
    },
    unmount: function () {
      root = null;
      if (verifyTimer != null) {
        window.clearTimeout(verifyTimer);
        verifyTimer = null;
      }
      if (radioStartTimer != null) {
        window.clearTimeout(radioStartTimer);
        radioStartTimer = null;
      }
    },
    onEvent: function (evt) {
      var name = (evt && evt.name) || '';
      // Setup mode: the radio coming up (or going down) settles the step.
      if (name.indexOf('aokie.dongle.') === 0 && setupActive() && !radioStarting) checkRadio();
      // Dongle plug/unplug/driver events refresh the pick list, but never
      // yank the wizard out of an install/verify step.
      if (name.indexOf('aokie.dongle.') === 0 && step === 'select' && !loading) {
        refresh();
      }
    },
  });
})();
