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
 * A driver step (the install, the restore, Linux's access rule) waits on a
 * person at a Windows or desktop prompt, so the plugin runs it off its command
 * loop: the command answers `accepted`, and `dongle.list`'s `driverJob` says
 * when it is done or why it failed. The tab polls that, keeps every button
 * that would start another step off while one runs, and finds a running step
 * again after the host gave up waiting (20 s) or the tab was left. It used to
 * wait on the command itself, which the host timed out while the elevated
 * helper still ran, and offered Install again: a second elevated install.
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
 *
 * Linux and macOS (`dongle.list` answers `driverModel: 'none'`): no driver is
 * installed there, Aokie opens the dongle as it is. The tab then has no
 * driver steps at all: it lists the dongle, says so, and when the radio has
 * not opened the dongle it says why in the radio's own words (on a Mac, that
 * macOS's Bluetooth is holding it).
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
  // The managed beta: the person ticked that Windows may trust a driver signed on this
  // computer. For ONE install only: cleared after every attempt and whenever the device or
  // the step changes, and sent as dongle.installDriver's acceptLocalSigning (never stored).
  var acceptSigning = false;
  var verifyTimer = null;
  var verifyAttempts = 0;
  // A driver step running in the plugin that this tab waits for: its kind
  // ('install' | 'restore'), the device it is for, and the poll timer.
  var jobKind = null;
  var jobTarget = null;
  var jobTimer = null;
  var jobWait = 0; // which wait a poll belongs to: a poll of one the tab has since dropped stops
  // The step the tab was waiting for when it was left ({kind, d}): on its return, its outcome is shown even when
  // it ended meanwhile. And the last step whose end this page has shown (kind@startedAt), so it is shown once.
  var leftJob = null;
  var seenJob = null;
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
  // "Other USB devices" unfolded (kept across renders, which rebuild the list).
  var otherOpen = false;

  function setupActive() {
    return !!(TABS.setup && TABS.setup.active());
  }

  /** The built-in Windows Bluetooth transport: no dongle is involved. (`auto` is the dongle whenever one is
   *  plugged in, as the plugin starts it, so its step is the dongle's.) */
  function builtInTransport() {
    return setupTransportKnown && !!TABS.transport && TABS.transport.mode() === 'native';
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

  /** "0bda:8771": a USB id as the catalog and the chip names below write it. */
  function hexKey(d) {
    var hex = function (n) {
      var s = Number(n).toString(16);
      while (s.length < 4) s = '0' + s;
      return s;
    };
    return hex(d.vid) + ':' + hex(d.pid);
  }

  /** The chip in each dongle of Aokie's catalog (aokie-core's dongle_catalog.rs). */
  var CHIPS = {
    '0a5c:21e8': 'Broadcom BCM20702',
    '0a5c:21ec': 'Broadcom BCM20702',
    '0bda:8771': 'Realtek RTL8761B',
    '0a12:0001': 'CSR8510 A10',
  };

  /** The chip's name where the device's own name does not say it already. */
  function chipOf(d) {
    var chip = CHIPS[hexKey(d)] || '';
    return chip && dongleLabel(d).toLowerCase().indexOf(chip.toLowerCase()) < 0 ? chip : '';
  }

  /** The catalog's tier for a device ('certified' or 'beta'), '' where the catalog does not list it. */
  function tierOf(d) {
    var known = (list && list.dongles) || [];
    for (var i = 0; i < known.length; i++) {
      if (Number(known[i].vid) === Number(d.vid) && Number(known[i].pid) === Number(d.pid)) return String(known[i].tier || '');
    }
    return '';
  }

  /** A device to offer as Aokie's dongle: one Aokie knows, one that still needs its driver (listed as a
   *  Bluetooth adapter), or one whose name says Bluetooth (Aokie's driver keeps the device's own name).
   *  Anything else on WinUSB is other hardware that uses the same driver (a lighting or keyboard controller,
   *  for one): it is listed apart, and nothing is offered for it. */
  function isCandidate(d) {
    return noDriver() || !!d.matchesCatalog || !d.driverBound || /bluetooth/i.test(dongleLabel(d));
  }

  function candidates() {
    return connectedDevices().filter(isCandidate);
  }

  /** The dongle Aokie's radio is up on, where that can be told: the only candidate with its driver. */
  function inUse(d) {
    if (!radioUp() || !d.driverBound || !isCandidate(d)) return false;
    var bound = candidates().filter(function (c) {
      return c.driverBound;
    });
    return bound.length === 1 && key(bound[0]) === key(d);
  }

  var ICON_BLUETOOTH = U.svg('<path d="m7 7 10 10-5 5V2l5 5L7 17"/>', 16);
  var ICON_DONE = U.svg('<path d="M20 6 9 17l-5-5"/>', 12);

  /** dongle.list's driverSigning: whether this build signs the driver on this computer. */
  function signing() {
    return (list && list.driverSigning) || null;
  }

  /** A system that installs no driver for the dongle (Linux, macOS): Aokie opens it as it is. */
  function noDriver() {
    return !!(list && list.driverModel === 'none');
  }

  /** Linux: the user opens the dongle once a udev rule gives it to them (`dongle.installDriver` puts it in place). */
  function udevAccess() {
    return !!(list && list.accessModel === 'udev');
  }

  /** Linux: a controller Aokie leaves to the system (not in the catalog, no unknown-dongle opt-in): a laptop's own
   *  Bluetooth is one. */
  function leftToSystem(d) {
    return udevAccess() && d.usable === false;
  }

  /** Linux: a dongle this user cannot open yet. */
  function accessNeeded(d) {
    return udevAccess() && !leftToSystem(d) && d.access === 'needed';
  }

  /** Linux: put the rule in place that lets this user open `d` (the desktop asks for the password). */
  function allow(d) {
    if (busy) return;
    busy = true;
    error = null;
    render();
    var allowed = function () {
      HOST.toast('success', 'Aokie may use ' + dongleLabel(d) + ' (USB ' + idLabel(d) + ') now. Start Aokie again to open it.');
      return refresh();
    };
    HOST.command('dongle.installDriver', { vid: d.vid, pid: d.pid }).then(
      function (data) {
        if (data && data.accepted) {
          waitForJob('install', d, allowed);
          return;
        }
        // (an older plugin answered once the rule was in place)
        busy = false;
        allowed().then(render);
      },
      function (e) {
        findRunningJob('install', d, e, allowed);
      }
    );
  }

  // ---- driver steps running in the plugin ---------------------------------

  /** dongle.list's driverJob: the driver step running or last run in the plugin. */
  function driverJob() {
    return (list && list.driverJob) || null;
  }

  function jobIsFor(job, kind, d) {
    return !!(job && job.kind === kind && (!d || (Number(job.vid) === Number(d.vid) && Number(job.pid) === Number(d.pid))));
  }

  function jobId(job) {
    return job ? job.kind + '@' + job.startedAt : null;
  }

  /** The step's device as the list has it now, else its ids (a rebind takes it off the list a while). */
  function jobDevice(job) {
    var conn = connectedDevices();
    for (var i = 0; i < conn.length; i++) {
      if (Number(conn[i].vid) === Number(job.vid) && Number(conn[i].pid) === Number(job.pid)) return conn[i];
    }
    return { vid: job.vid, pid: job.pid };
  }

  /** What follows a `kind` step for `d` that is done, as a wait for it does. */
  function onDoneFor(kind, d) {
    if (kind === 'install' && !noDriver()) {
      return function () {
        verifySelected(d);
      };
    }
    if (kind === 'install') {
      return function () {
        HOST.toast('success', 'Aokie may use ' + dongleLabel(d) + ' (USB ' + idLabel(d) + ') now. Start Aokie again to open it.');
        render();
      };
    }
    if (kind === 'restore') return restored;
    return function () {
      render();
    };
  }

  /** The step being waited for is gone from the plugin: Aokie was started again while it ran (the plugin finds a
   *  helper still running and reports it, so this is one that ended meanwhile). An install is checked as after any
   *  install (is the dongle on WinUSB now?); for the rest the screen says so, and Install is not offered as though
   *  nothing had run. */
  function jobVanished(kind, d) {
    if (kind === 'install' && !noDriver() && d) {
      step = 'install';
      selectedKey = key(d);
      verifySelected(d);
      return;
    }
    error = 'Aokie was started again while the driver step ran, so it cannot say how it went. Rescan to see the dongle as it is now.';
    render();
  }

  /** Wait for the plugin's `kind` step for `d` to end (however long the person takes over its prompt), polling
   *  dongle.list every 2 s: then `onDone(job)`, or its error on the screen. */
  function waitForJob(kind, d, onDone) {
    busy = true;
    error = null;
    jobKind = kind;
    jobTarget = d;
    var wait = ++jobWait;
    render();
    var poll = function () {
      jobTimer = null;
      if (!root || wait !== jobWait) return; // tab left: mount finds the running step again
      refresh().then(function (data) {
        if (!root || wait !== jobWait) return;
        var job = data && data.driverJob;
        // (no answer this time, or the step is still on: ask again)
        if (data === null || (jobIsFor(job, kind, d) && job.state === 'running')) {
          jobTimer = window.setTimeout(poll, 2000);
          return;
        }
        busy = false;
        jobKind = null;
        jobTarget = null;
        if (job) seenJob = jobId(job);
        if (jobIsFor(job, kind, d) && job.state === 'done') {
          onDone(job);
          return;
        }
        if (!job) {
          jobVanished(kind, d);
          return;
        }
        error = job.error || 'The driver step ended without saying how it went. Rescan, then try again.';
        if (kind === 'install' && !noDriver()) step = 'install';
        render();
      });
    };
    jobTimer = window.setTimeout(poll, 1000);
  }

  /** A driver command came back with an error: the host's "did not respond" after 20 s while the step still runs
   *  in the plugin, or the plugin's refusal. Ask dongle.list which, and wait for a step that is running. */
  function findRunningJob(kind, d, e, onDone) {
    refresh().then(function (data) {
      var job = data && data.driverJob;
      if (jobIsFor(job, kind, d) && job.state === 'running') {
        waitForJob(kind, d, onDone);
        return;
      }
      busy = false;
      error = errMsg(e);
      if (kind === 'install' && !noDriver()) step = 'install';
      render();
    });
  }

  /** On mount: a step the plugin is still running (started before the tab was left, or by another screen) is
   *  waited for again, its buttons off until it ends. */
  function resumeRunningJob() {
    var job = driverJob();
    if (!job || job.state !== 'running' || jobKind) return false;
    leftJob = null;
    var d = jobDevice(job);
    if (job.kind === 'install' && !noDriver()) {
      step = 'install';
      selectedKey = key(d);
      waitForJob('install', d, onDoneFor('install', d));
    } else if (job.kind === 'install' || job.kind === 'restore') {
      waitForJob(job.kind, d, onDoneFor(job.kind, d));
    } else {
      // Another step (the certificates' removal): its buttons off until it ends.
      waitForJob(job.kind, null, onDoneFor(job.kind, null));
    }
    return true;
  }

  /** On mount, where no step runs: one that ended while the tab was away is shown as a wait would have shown it
   *  (verified, or its error), once. That is the step the tab was waiting for when it was left, or one that ended
   *  in the last ten minutes that this page has not shown (begun by another screen, or before the screen was
   *  opened again). Without this the tab came back on Install, as if nothing had run. */
  function showFinishedJob() {
    var left = leftJob;
    leftJob = null;
    var job = driverJob();
    if (jobKind) return false;
    if (!job) {
      if (left) jobVanished(left.kind, left.d);
      return !!left;
    }
    if (job.state === 'running' || jobId(job) === seenJob) return false;
    seenJob = jobId(job);
    var finished = Date.parse(job.finishedAt || '');
    var recent = !isNaN(finished) && Date.now() - finished < 10 * 60 * 1000;
    if (!(left && jobIsFor(job, left.kind, left.kind === 'removeCerts' ? null : left.d)) && !recent) return false;
    var d = job.kind === 'install' || job.kind === 'restore' ? jobDevice(job) : null;
    if (job.state === 'done') {
      onDoneFor(job.kind, d)(job);
      return true;
    }
    error = job.error || 'The driver step ended without saying how it went. Rescan, then try again.';
    if (job.kind === 'install' && !noDriver() && d) {
      step = 'install';
      selectedKey = key(d);
    }
    render();
    return true;
  }

  /** A restore the plugin read back on another driver. */
  function restored(job) {
    var driver = (job && job.result && job.result.driver) || 'the standard Windows Bluetooth driver';
    HOST.toast('success', 'Windows driver restored — the dongle is on ' + driver + ' again.');
    return refresh();
  }

  /** A Realtek dongle whose firmware Aokie could not load at its last start runs its ROM, which phones do not see. */
  function firmwareHtml() {
    var fw = list && list.firmware;
    if (!fw || (fw.state !== 'missing' && fw.state !== 'failed')) return '';
    var why =
      fw.state === 'missing'
        ? 'its firmware files are not there' + (fw.detail ? ' (' + esc(fw.detail) + ')' : '')
        : 'the dongle refused it' + (fw.detail ? ' (' + esc(fw.detail) + ')' : '');
    return (
      '<div class="rcp-callout is-warn" role="status">' + ICONS.alert +
      '<span>Aokie could not load the ' + esc(fw.chip || 'Realtek') + ' firmware when it last started: ' + why +
      '. The dongle runs its ROM instead, and phones do not see it when pairing. ' +
      (fw.state === 'missing'
        ? 'Reinstall Aokie (its bundle carries the firmware), then start it again.'
        : 'Unplug the dongle, plug it back in, then start Aokie again.') +
      '</span></div>'
    );
  }

  /** The dongle chosen for Aokie (dongle.setPreferred), as dongle.list reports it. */
  function isPreferred(d) {
    var p = list && list.preferred;
    return !!(p && d && Number(p.vid) === Number(d.vid) && Number(p.pid) === Number(d.pid));
  }

  /** Choose the dongle Aokie opens (no-driver systems, several controllers). */
  function prefer(d) {
    busy = true;
    error = null;
    render();
    HOST.command('dongle.setPreferred', { vid: d.vid, pid: d.pid })
      .then(
        function () {
          HOST.toast('success', 'Aokie will use ' + dongleLabel(d) + ' (USB ' + idLabel(d) + '). Start Aokie again to open it.');
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

  /** The managed beta, not pre-authorised: the person must accept before installing. */
  function signingNeedsAcceptance() {
    var sg = signing();
    return !!(sg && sg.acceptanceRequired === true);
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
        // (a step being waited for keeps its error slot, and its device: a rebind takes it off the list a while)
        if (!jobKind) error = null;
        // Keep the selection pinned to the same physical id across refreshes.
        if (selectedKey && !selectedDevice() && !jobKind) selectedKey = null;
        // Nothing picked yet: the dongle chosen for Aokie, else the only one there is to pick.
        if (!selectedKey) {
          var offered = candidates();
          var chosen = offered.filter(isPreferred);
          if (chosen.length === 1) selectedKey = key(chosen[0]);
          else if (offered.length === 1) selectedKey = key(offered[0]);
        }
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
    // (one driver step at a time: the plugin refuses a second, and its button is off)
    if (!sel || busy) return;
    busy = true;
    error = null;
    render();
    var request = { vid: sel.vid, pid: sel.pid };
    if (signingNeedsAcceptance()) {
      if (!acceptSigning) {
        busy = false;
        render();
        return;
      }
      request.acceptLocalSigning = true;
    }
    // One acceptance, one install: the next one asks again.
    acceptSigning = false;
    var installed = function () {
      verifySelected(sel);
    };
    HOST.command('dongle.installDriver', request).then(
      function (data) {
        if (data && data.accepted) {
          // The plugin runs it: wait for the step (a declined UAC prompt fails it, with a human error).
          waitForJob('install', sel, installed);
          return;
        }
        // (an older plugin answered once the install was over)
        busy = false;
        installed();
      },
      function (e) {
        findRunningJob('install', sel, e, installed);
      }
    );
  }

  function restore(d) {
    if (busy) return;
    busy = true;
    error = null;
    restoreArm = null;
    render();
    HOST.command('dongle.restoreDriver', { vid: d.vid, pid: d.pid }).then(
      function (data) {
        if (data && data.accepted) {
          // Done only once the plugin reads the dongle back on a driver other than WinUSB.
          waitForJob('restore', d, restored);
          return;
        }
        busy = false;
        restored(data).then(render);
      },
      function (e) {
        findRunningJob('restore', d, e, restored);
      }
    );
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
      // render() reports the progress first, then the step is done.
      render();
      reportSetup();
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
          render();
          reportSetup();
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
    else if (step === 'select') S.progress(0, noDriver() ? 'Plug in your Bluetooth dongle' : 'Choose your Bluetooth dongle');
    else if (step === 'install') S.progress(1 / 3, 'Install the WinUSB driver');
    else if (step === 'verify') S.progress(2 / 3, 'Verifying the driver');
    else S.progress(2 / 3, 'Start Aokie with the dongle');
  }

  /** Setup mode: the radio's state above the wizard's steps. */
  function setupStatusHtml() {
    if ((!setupActive() && !noDriver()) || setupRadio === undefined) return '';
    if (radioUp()) {
      return (
        '<p class="rcp-status-ok rcp-radio-up">' + ICONS.check + ' Aokie’s radio is up on the dongle' +
        (setupRadio.localAddress ? ' (' + esc(setupRadio.localAddress) + ')' : '') +
        (setupActive() ? '. Press Next to pair your phone.' : '.') + '</p>'
      );
    }
    // Consent keeps the radio off (enforce mode): starting Aokie again would
    // not open the dongle, so say why instead, as the Overview does.
    if (setupRadio && setupRadio.paused && setupRadio.blockedBy === 'consent' && !radioStarting) {
      return (
        '<div class="rcp-callout is-warn">' + ICONS.alert +
        '<span>Aokie’s radio is off until consent is given: ' +
        esc(setupRadio.reason || 'consent must be reviewed before the radio can start') +
        '. Go back to the consent step, then return here.</span></div>'
      );
    }
    // A dongle with its driver (not other hardware on WinUSB: that says nothing of the dongle).
    var bound = false;
    var conn = candidates();
    for (var i = 0; i < conn.length; i++) {
      if (conn[i].driverBound) bound = true;
    }
    if (!bound && !radioStarting && !radioStartError) return '';
    // No driver step: the reason is whatever the radio said when it tried (on
    // a Mac, that macOS's own Bluetooth holds the dongle), or that the dongle
    // was plugged in after Aokie started.
    var why = noDriver()
      ? 'Aokie’s radio has not opened the dongle' +
        (setupRadio && setupRadio.error
          ? ': ' + esc(setupRadio.error) + '. If that is put right, start Aokie again to open the dongle.'
          : '. If it was plugged in after Aokie started, start Aokie again to open it. It takes a few seconds.')
      : 'The dongle has its driver, but Aokie’s radio has not opened it: Aokie started before the driver was there. ' +
        'Start Aokie again to open the dongle. It takes a few seconds.';
    return (
      '<div class="rcp-callout is-warn">' + ICONS.alert +
      '<span>' + why + '</span></div>' +
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

  /** The driver wizard's three steps, as marks: done, the one open, still to come (Windows only: a system
   *  that installs no driver has no steps). A picked dongle that has its driver already is all three done. */
  function stepperHtml() {
    if (noDriver()) return '';
    var sel = selectedDevice();
    var at =
      step === 'select' ? (sel && sel.driverBound && isCandidate(sel) ? 3 : 0) : step === 'install' ? 1 : step === 'verify' ? 2 : 3;
    var names = ['Choose the dongle', 'Install its driver', 'Check it'];
    var items = [];
    for (var i = 0; i < names.length; i++) {
      var state = i < at ? 'is-done' : i === at ? 'is-current' : 'is-todo';
      items.push(
        '<li class="' + state + '"' + (i === at ? ' aria-current="step"' : '') + '>' +
          '<span class="rcp-stepper__mark">' + (i < at ? ICON_DONE : String(i + 1)) + '</span>' +
          '<span class="rcp-stepper__name">' + esc(names[i]) + '</span>' +
          '<span class="rcp-sr">' + (i < at ? ', done' : i === at ? ', now' : '') + '</span>' +
          '</li>'
      );
    }
    return '<ol class="rcp-stepper" aria-label="Setting up the dongle">' + items.join('') + '</ol>';
  }

  /** One dongle to pick: what it is (the chip, where the catalog names it), its USB id, and how it stands. */
  function dongleRowHtml(d) {
    var isSel = selectedKey === key(d);
    var chip = chipOf(d);
    var tier = tierOf(d);
    var badges =
      (inUse(d) ? '<span class="rcp-badge is-ok">in use</span>' : '') +
      (d.matchesCatalog
        ? '<span class="rcp-badge ' + (tier === 'certified' ? 'is-ok' : 'is-neutral') + '">' + (tier ? esc(tier) : 'supported') + '</span>'
        : '<span class="rcp-badge is-neutral">unknown chipset</span>') +
      (leftToSystem(d)
        ? '<span class="rcp-badge is-neutral">left to the system</span>'
        : accessNeeded(d)
          ? '<span class="rcp-badge is-pending">access needed</span>'
        : noDriver()
        ? '<span class="rcp-badge is-ok">no driver needed</span>'
        : d.driverBound
          ? '<span class="rcp-badge is-ok">WinUSB installed</span>'
          : '<span class="rcp-badge is-pending">driver required</span>');
    return (
      '<label class="rcp-wizard-row' + (isSel ? ' is-selected' : '') + '">' +
      '<input type="radio" name="dongle-pick" data-pick="' + esc(key(d)) + '"' + (isSel ? ' checked' : '') + ' />' +
      '<span class="rcp-wizard-row__icon">' + ICON_BLUETOOTH + '</span>' +
      '<span class="rcp-wizard-row__name">' + esc(dongleLabel(d)) +
      '<small>' + (chip ? esc(chip) + ' · ' : '') + 'USB ' + esc(idLabel(d)) + '</small></span>' +
      '<span class="rcp-wizard-row__badges">' + badges + '</span>' +
      '</label>'
    );
  }

  /** Other hardware on the WinUSB driver: listed apart and folded, with nothing offered for it. */
  function otherDevicesHtml(others) {
    if (!others.length) return '';
    var items = others.map(function (d) {
      return '<li><span>' + esc(dongleLabel(d)) + '</span><small>USB ' + esc(idLabel(d)) + '</small></li>';
    });
    return (
      '<details class="rcp-other-usb"' + (otherOpen ? ' open' : '') + '>' +
      '<summary data-act="dg-other-toggle">Other USB devices on the WinUSB driver (' + others.length + ')</summary>' +
      '<p class="rcp-step-meta">Not Bluetooth dongles: other hardware uses the same driver (a lighting or keyboard ' +
      'controller, for one). Aokie changes nothing on them.</p>' +
      '<ul>' + items.join('') + '</ul>' +
      '</details>'
    );
  }

  /** A step's buttons in one bar, the way on at its right (in OAIY's wizard it stays in view). */
  function barHtml(left, right) {
    return (
      '<div class="rcp-actions rcp-dongle__bar">' +
      left.join('') +
      (right.length ? '<span class="rcp-dongle__bar-end">' + right.join('') + '</span>' : '') +
      '</div>'
    );
  }

  function errorHtml() {
    if (!error) return '';
    return (
      '<div class="rcp-error" role="alert">' + esc(error) +
      (step === 'verify'
        ? ' <button type="button" class="rcp-button" data-act="dg-verify-again"' + (busy ? ' disabled' : '') + '>Check again</button>'
        : '') +
      '</div>'
    );
  }

  function selectStepHtml() {
    var conn = connectedDevices();
    var offered = candidates();
    var others = conn.filter(function (d) {
      return !isCandidate(d);
    });
    var html = [];
    if (list && list.liveEnumeration === false) {
      html.push(
        '<div class="rcp-callout is-warn">' + ICONS.alert + '<span>Live USB scan unavailable' +
          (list.note ? ': ' + esc(list.note) : '') + '.</span></div>'
      );
    }
    if (list === null && loading) {
      html.push('<p class="rcp-loading">Scanning USB devices…</p>');
    } else if (offered.length === 0) {
      html.push(
        '<p class="rcp-step-meta">' +
          (loading
            ? 'Scanning USB devices…'
            : 'No supported dongle detected. Plug the USB Bluetooth dongle in, then rescan. ' +
              'Supported chipsets: Broadcom BCM20702 (certified), Realtek RTL8761 and CSR8510 (beta).') +
          '</p>'
      );
    } else {
      if (offered.length > 1 && !selectedDevice()) html.push('<p class="rcp-step-meta">Choose the dongle Aokie should use.</p>');
      html.push('<div class="rcp-wizard-list">' + offered.map(dongleRowHtml).join('') + '</div>');
    }
    html.push(otherDevicesHtml(others));

    var sel = selectedDevice();
    if (sel && !isCandidate(sel)) sel = null;
    var right = [];
    if (sel && leftToSystem(sel)) {
      // (not offered to the radio: on Linux a computer's own Bluetooth is a USB controller like a dongle)
      html.push(
        '<p class="rcp-step-meta">This is not a dongle Aokie knows, and this computer’s own Bluetooth is a controller ' +
          'like it, so Aokie leaves it to the system. Plug in a supported USB Bluetooth dongle for Aokie.</p>'
      );
    } else if (sel && accessNeeded(sel)) {
      html.push(
        '<p class="rcp-step-meta">Linux gives a USB device to the person using the computer once a rule says so. Aokie ' +
          'puts that rule in place for its dongles (your password is asked), and takes the dongle from the system’s ' +
          'Bluetooth only while Aokie runs.</p>'
      );
      right.push(
        '<button type="button" class="rcp-button is-primary" data-act="dg-allow" data-pick="' + esc(key(sel)) + '"' +
          (busy ? ' disabled' : '') + '>Allow Aokie to use this dongle</button>'
      );
    } else if (sel && noDriver()) {
      html.push(
        '<p class="rcp-status-ok">' + ICONS.check + ' This computer needs no driver for the dongle: Aokie opens it as it is.</p>'
      );
      // No driver marks a dongle as Aokie's here, so with several Bluetooth
      // controllers connected (a laptop's own is one) the person says which.
      if (conn.length > 1 && !isPreferred(sel)) {
        right.push(
          '<button type="button" class="rcp-button is-primary" data-act="dg-prefer" data-pick="' + esc(key(sel)) + '"' +
            (busy ? ' disabled' : '') + '>Use this dongle</button>'
        );
      } else if (isPreferred(sel)) {
        html.push('<p class="rcp-step-meta">Aokie is set to use this one.</p>');
      }
    } else if (sel && sel.driverBound) {
      // The radio's own line above says it is ready where it is up.
      if (!radioUp()) {
        html.push(
          '<p class="rcp-status-ok">' + ICONS.check + " This dongle already has the WinUSB driver — it's ready for Aokie.</p>"
        );
      }
      right.push(
        '<button type="button" class="rcp-button is-warn" data-act="dg-restore-arm" data-pick="' + esc(key(sel)) + '"' +
          (busy ? ' disabled' : '') + '>Restore Windows driver</button>'
      );
    } else if (sel) {
      right.push('<button type="button" class="rcp-button is-primary" data-act="dg-continue">Continue</button>');
    }

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
    html.push(errorHtml());
    html.push(
      barHtml(
        ['<button type="button" class="rcp-button" data-act="dg-rescan"' + (loading ? ' disabled' : '') + '>' + (loading ? 'Scanning…' : 'Rescan') + '</button>'],
        right
      )
    );
    return html.join('');
  }

  function installStepHtml() {
    // (while its install runs the device may drop off the list: the rebind re-enumerates it)
    var sel = selectedDevice() || (jobKind === 'install' ? jobTarget : null);
    if (!sel) {
      return (
        '<p class="rcp-error">The selected dongle disappeared — rescan and pick it again.</p>' +
        barHtml(['<button type="button" class="rcp-button" data-act="dg-back">Choose another dongle</button>'], [])
      );
    }
    var chip = chipOf(sel);
    return (
      '<div class="rcp-wizard-row is-selected is-static">' +
      '<span class="rcp-wizard-row__icon">' + ICON_BLUETOOTH + '</span>' +
      '<span class="rcp-wizard-row__name">' + esc(dongleLabel(sel)) +
      '<small>' + (chip ? esc(chip) + ' · ' : '') + 'USB ' + esc(idLabel(sel)) + '</small></span>' +
      '</div>' +
      (!sel.matchesCatalog
        ? "<p class=\"rcp-step-meta\">This chipset isn't in the supported catalog — the host will refuse the install unless the unknown-dongle override is set.</p>"
        : '') +
      '<p class="rcp-step-meta">Windows will show a <strong>User Account Control</strong> prompt — choose <strong>Yes</strong>. ' +
      'Only this exact device is rebound (the installer pins its hardware identity), and you can restore the standard ' +
      'Windows driver at any time.</p>' +
      (jobKind === 'install'
        ? '<p class="rcp-step-meta" role="status">Installing: answer the Windows prompt if one is open (it may be behind ' +
          'this window). The install can take a minute or two; this waits for it to finish.</p>'
        : '') +
      signingHtml() +
      errorHtml() +
      barHtml(
        ['<button type="button" class="rcp-button" data-act="dg-back"' + (busy ? ' disabled' : '') + '>Choose another dongle</button>'],
        [
          '<button type="button" class="rcp-button is-primary" data-act="dg-install"' +
            (busy || (signingNeedsAcceptance() && !acceptSigning) ? ' disabled' : '') + '>' +
            (busy ? 'Installing — watch for the Windows prompt…' : 'Install WinUSB driver') +
            '</button>',
        ]
      )
    );
  }

  /** The managed beta's driver is signed on this computer: say what that means, and ask. */
  function signingHtml() {
    var sg = signing();
    if (!sg || sg.flavour !== 'managed-beta') return '';
    if (!sg.acceptanceRequired) {
      // Not asked: an administrator pre-authorised the machine, or this is a debug build of
      // the managed beta, which signs on a developer's own machine without asking.
      if (!sg.preauthorised) return '';
      return (
        '<p class="rcp-notice rcp-notice--warn" role="note"><strong>Managed beta:</strong> this computer’s administrator ' +
        'has allowed drivers signed on this computer, so Windows will trust the certificate Aokie makes here for the dongle’s driver.</p>'
      );
    }
    return (
      '<div class="rcp-callout is-warn" role="group" aria-label="A driver signed on this computer">' +
      ICONS.alert +
      '<span><strong>This is the managed beta of Aokie.</strong> Its dongle driver is not signed by Microsoft yet, so Aokie ' +
      'signs it on this computer: its installer makes a signing certificate here, signs this dongle’s driver with it, deletes ' +
      'the certificate’s private key, and Windows then trusts that certificate on this whole computer (in Local Machine ' +
      '<em>Root</em> and <em>Trusted Publishers</em>). It cannot sign anything else, since its private key is gone, and it ' +
      'stays trusted after the install. <strong>Restore driver</strong> gives the dongle back to the standard Windows driver.' +
      '<label class="rcp-check" style="display: block; margin-top: 8px;">' +
      '<input type="checkbox" data-act="dg-accept-signing"' + (acceptSigning ? ' checked' : '') + (busy ? ' disabled' : '') + ' /> ' +
      'I understand, and want Windows to trust a driver signed on this computer for this dongle.' +
      '</label></span>' +
      '</div>'
    );
  }

  function verifyStepHtml() {
    return (
      '<p class="rcp-step-meta">Checking that the dongle came back with its WinUSB driver…</p>' +
      '<span class="rcp-dongle__wait" aria-hidden="true"><i></i></span>' +
      errorHtml()
    );
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
      errorHtml() +
      barHtml(['<button type="button" class="rcp-button" data-act="dg-again">Set up another dongle</button>'], [])
    );
  }

  function render() {
    if (!root) return;
    reportProgress();
    var setup = setupActive();
    if (setup && builtInTransport()) {
      root.innerHTML = builtInHtml();
      return;
    }
    var body;
    if (step === 'select') body = selectStepHtml();
    else if (step === 'install') body = installStepHtml();
    else if (step === 'verify') body = verifyStepHtml();
    else body = doneStepHtml();

    // In OAIY's wizard the step's own heading says what this is: the card holds the radio's state, the
    // steps and the step open, its buttons last.
    root.innerHTML =
      '<section class="rcp-card rcp-dongle">' +
      (setup
        ? ''
        : '<div class="rcp-card__heading">' +
          '<div class="rcp-card__heading-copy">' +
          '<small>Dongle setup</small>' +
          '<h3>' + (noDriver() ? 'USB Bluetooth dongle' : 'USB Bluetooth dongle driver') + '</h3>' +
          '</div>' +
          '</div>') +
      '<div class="rcp-card__body">' +
      (setup
        ? ''
        : noDriver()
          ? '<p class="rcp-inline-note">Plug the USB Bluetooth dongle in. This computer needs no driver for it: ' +
            'Aokie opens the dongle when it starts.</p>'
          : '<p class="rcp-inline-note">Guided setup for the USB Bluetooth dongle: pick the device, install the WinUSB ' +
            "driver (one Windows prompt), and verify it's ready.</p>") +
      setupStatusHtml() +
      firmwareHtml() +
      stepperHtml() +
      body +
      '</div>' +
      (setup
        ? ''
        : noDriver()
          ? '<p class="rcp-footnote">Aokie talks to the dongle directly over USB. Nothing is installed on this computer ' +
            'for it, and closing Aokie gives the dongle back.</p>'
          : '<p class="rcp-footnote">All device work happens in the plugin and its elevated helper — this screen only ' +
            'sequences it. Restoring the Windows driver at any time hands the dongle back to BTHUSB.</p>') +
      '</section>' +
      // Recovery for a working receptionist, not part of setting one up.
      (setup ? '' : resetCardHtml());
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
        acceptSigning = false;
        render();
      }
    });
    el.addEventListener('click', function (e) {
      var btn = e.target && e.target.closest ? e.target.closest('[data-act]') : null;
      if (!btn || btn.disabled) return;
      var act = btn.getAttribute('data-act');
      if (act === 'dg-other-toggle') {
        // The list is rebuilt on every render: its fold follows this state, not the browser's own toggle.
        e.preventDefault();
        otherOpen = !otherOpen;
        render();
      } else if (act === 'dg-rescan') refresh();
      else if (act === 'dg-continue') {
        step = 'install';
        error = null;
        render();
      } else if (act === 'dg-back') {
        step = 'select';
        acceptSigning = false;
        render();
      } else if (act === 'dg-accept-signing') {
        // A click flips the box (the browser has already flipped its own mark); the
        // rendered box follows this state.
        acceptSigning = !acceptSigning;
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
      } else if (act === 'dg-prefer') {
        var chosen = selectedDevice();
        if (chosen && key(chosen) === btn.getAttribute('data-pick')) prefer(chosen);
      } else if (act === 'dg-allow') {
        var granted = selectedDevice();
        if (granted && key(granted) === btn.getAttribute('data-pick')) allow(granted);
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
        acceptSigning = false;
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
      refresh().then(function (data) {
        if (!root) return;
        // A driver step still running in the plugin: wait for it again, its buttons off. One that ended while the
        // tab was away: its outcome. (No answer from the plugin: the step the tab left is looked for next time.)
        if (!resumeRunningJob() && data) showFinishedJob();
        // Where there is no driver step, the radio's state is the tab's news.
        if (noDriver() && !setupActive()) checkRadio();
      });
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
      if (jobTimer != null) {
        window.clearTimeout(jobTimer);
        jobTimer = null;
      }
      if (jobKind) {
        // The step goes on in the plugin; mount finds it again, or its outcome if it ended meanwhile (a poll still
        // in flight stops).
        leftJob = { kind: jobKind, d: jobTarget };
        jobWait += 1;
        jobKind = null;
        jobTarget = null;
        busy = false;
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
