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
 * Sandbox deltas: the wizard renders as a whole tab (never collapsed), the
 * restore confirm is INLINE (no host dialog), and the done step points at
 * the Phone setup TAB (pairing no longer lives "below" on the same card).
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
    render();
    var targetKey = key(target);
    var poll = function () {
      refresh().then(function (data) {
        if (!root) return; // tab left mid-verify
        var now = null;
        var conn = (data && data.connected) || [];
        for (var i = 0; i < conn.length; i++) {
          if (key(conn[i]) === targetKey) now = conn[i];
        }
        if (now && now.driverBound) {
          step = 'done';
          selectedKey = targetKey;
          HOST.toast(
            'success',
            'Dongle driver installed — ' + dongleLabel(target) + ' is now bound to WinUSB and ready for Aokie.'
          );
          render();
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
      '<p class="rcp-step-meta">Next: open the ' +
      '<button type="button" class="rcp-link-btn" data-tabgo="phone">Phone setup tab</button>' +
      ' and pair your handset (Bluetooth pairing → "Pair a phone").</p>' +
      '<div class="rcp-actions">' +
      '<button type="button" class="rcp-button" data-act="dg-again">Set up another dongle</button>' +
      '</div>'
    );
  }

  function render() {
    if (!root) return;
    var body;
    if (step === 'select') body = selectStepHtml();
    else if (step === 'install') body = installStepHtml();
    else if (step === 'verify') body = verifyStepHtml();
    else body = doneStepHtml();

    root.innerHTML =
      '<section class="rcp-card">' +
      '<div class="rcp-card__heading">' +
      '<div class="rcp-card__heading-copy">' +
      '<small>DONGLE SETUP</small>' +
      '<h3>USB Bluetooth dongle driver</h3>' +
      '</div>' +
      '</div>' +
      '<div class="rcp-card__body">' +
      '<p class="rcp-inline-note">Guided setup for the USB Bluetooth dongle: pick the device, install the WinUSB ' +
      "driver (one Windows prompt), and verify it's ready.</p>" +
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
      '</section>';
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
        var target = selectedDevice();
        if (target) verifySelected(target);
      } else if (act === 'dg-again') {
        step = 'select';
        error = null;
        refresh();
      }
    });
  }

  TABS.register('dongle', {
    mount: function (el) {
      root = el;
      wire(el);
      render();
      refresh();
    },
    unmount: function () {
      root = null;
      if (verifyTimer != null) {
        window.clearTimeout(verifyTimer);
        verifyTimer = null;
      }
    },
    onEvent: function (evt) {
      var name = (evt && evt.name) || '';
      // Dongle plug/unplug/driver events refresh the pick list, but never
      // yank the wizard out of an install/verify step.
      if (name.indexOf('aokie.dongle.') === 0 && step === 'select' && !loading) {
        refresh();
      }
    },
  });
})();
