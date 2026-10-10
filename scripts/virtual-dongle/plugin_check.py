#!/usr/bin/env python3
"""The whole plugin on the stand-in dongle.

Starts `aokie-plugin` the way OAIY Desktop does (JSON-RPC lines over
stdio, real mode: no dev flag), records consent for Bluetooth, and checks
that the plugin's own radio thread opens the stand-in through libusb,
brings the controller up, and answers the commands a setup screen sends:
`dongle.diagnostics` (radio up, with the stand-in's address), `dongle.list`
(the dongle listed, no driver to install), `phone.status`, a pairing window
opened and closed. Then it asks the plugin to shut down and expects it to
go cleanly, which gives the dongle back.

Nothing is on the air: no phone can pair with the stand-in, so calls and
texts are beyond this. It is the step before them: "the plugin starts and
its radio is up" on a system that is not Windows.

Usage: plugin_check.py PATH/TO/aokie-plugin

AOKIE_PLUGIN_CHECK_WAIT: how many seconds to wait for the radio (default
30). A plugin built with voice fetches its speech models before its radio
thread looks at the dongle's events, on a first start: give it minutes. And
give this a plugin from a folder scripts/bundle-unix.sh laid out, which has
ONNX Runtime 1.25.0 beside it: without that runtime a plugin that has its
models aborts on its way out (the README says why).
AOKIE_PLUGIN_CHECK_LOG: how many of the last lines of the plugin's own log
to print when all went well (they are printed anyway when something did not).
"""
import json
import os
import queue
import subprocess
import sys
import tempfile
import threading
import time

STAND_IN = "02:A0:C1:E0:00:01"


class Plugin:
    def __init__(self, exe, data_dir):
        env = dict(os.environ)
        env["FORMLOGIC_PLUGIN_DATA_DIR"] = data_dir
        for name in ("FORMLOGIC_DEV_MODE", "FORMLOGIC_CONSENT_VERIFY_KEY"):
            env.pop(name, None)
        self.log = open(os.path.join(data_dir, "plugin-stderr.log"), "wb")
        self.proc = subprocess.Popen([exe, "--stdio"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=self.log, env=env)
        self.answers = {}
        self.events = queue.Queue()
        self.arrived = threading.Condition()
        self.next_id = 0
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        for raw in self.proc.stdout:
            try:
                line = json.loads(raw)
            except ValueError:
                continue
            if "id" in line and "method" not in line:
                with self.arrived:
                    self.answers[line["id"]] = line
                    self.arrived.notify_all()
            else:
                self.events.put(line)

    def rpc(self, method, params, wait=20.0):
        self.next_id += 1
        ident = self.next_id
        message = {"jsonrpc": "2.0", "id": ident, "method": method, "params": params}
        self.proc.stdin.write((json.dumps(message) + "\n").encode())
        self.proc.stdin.flush()
        deadline = time.monotonic() + wait
        with self.arrived:
            while ident not in self.answers:
                left = deadline - time.monotonic()
                if left <= 0 or self.proc.poll() is not None:
                    raise RuntimeError("%s: no answer (plugin exit code: %s)" % (method, self.proc.poll()))
                self.arrived.wait(min(left, 0.5))
            return self.answers.pop(ident)

    def command(self, name, payload=None, wait=20.0):
        """A connector command. Returns its data, or raises with the plugin's own message."""
        # (a request id: the commands that change something are journalled by it)
        params = {"connectorId": "aokie", "command": name, "requestId": "plugin-check-%d" % (self.next_id + 1)}
        if payload is not None:
            params["payload"] = payload
        answer = self.rpc("connector.request", params, wait)
        if "error" in answer:
            raise RuntimeError("%s: %s" % (name, json.dumps(answer["error"])[:400]))
        result = answer.get("result") or {}
        if result.get("ok") is False:
            raise RuntimeError("%s: %s" % (name, json.dumps(result.get("error") or result)[:400]))
        return result.get("data", result)


def check(condition, what):
    if not condition:
        raise AssertionError(what)
    print("  ok: " + what, flush=True)


def main():
    exe = sys.argv[1]
    data_dir = tempfile.mkdtemp(prefix="aokie-plugin-check-")
    plugin = Plugin(exe, data_dir)
    try:
        init = plugin.rpc("plugin.init", {"desktopVersion": "0.1.0", "pluginApiVersion": 1, "devMode": False, "dataDir": data_dir})
        check("error" not in init, "plugin.init answered")

        # With no consent the radio stays off: the dongle is not touched.
        consent = plugin.command("consent.get")
        check(consent.get("blocked") is not None or consent.get("grant") is None, "no consent yet, so no radio")
        plugin.command("consent.set", {
            "version": consent.get("requiredVersion", 1),
            "scopes": {"bluetooth": True, "transcription": False, "sms": True, "contacts": True},
        })
        print("  consent recorded for Bluetooth: the radio may start", flush=True)

        radio, last_error = None, None
        deadline = time.monotonic() + float(os.environ.get("AOKIE_PLUGIN_CHECK_WAIT", "30"))
        while time.monotonic() < deadline:
            try:
                radio = plugin.command("dongle.diagnostics").get("radio") or {}
                if radio.get("initialized"):
                    break
                last_error = radio.get("error")
            except RuntimeError as error:
                last_error = str(error)
            time.sleep(0.5)
        check(bool(radio and radio.get("initialized")), "the plugin's radio is up (last word otherwise: %s)" % last_error)
        check(radio.get("localAddress") == STAND_IN, "it is up on the stand-in (%s)" % radio.get("localAddress"))

        listed = plugin.command("dongle.list")
        check(listed.get("liveEnumeration") is True, "dongle.list scans live USB devices")
        ours = [d for d in listed.get("connected", []) if d.get("vid") == 0x1209 and d.get("pid") == 0x0001]
        check(len(ours) == 1, "the stand-in is listed (%d device(s) in all)" % len(listed.get("connected", [])))
        if sys.platform != "win32":
            check(listed.get("driverModel") == "none", "this system installs no driver for the dongle")
            check(ours[0].get("driverBound") is True, "so the dongle is ready as far as drivers go")

        status = plugin.command("phone.status")
        check(status.get("connected") is False, "no phone is connected (none can be: nothing is on the air)")

        check(status.get("pairingOpen") is False, "and no pairing window is open")
        plugin.command("phone.startPairing", {})
        time.sleep(0.5)
        opened = plugin.command("phone.status")
        check(opened.get("pairingOpen") is True and opened.get("pairingSecondsRemaining", 0) > 0,
              "a pairing window opened (%s s left)" % opened.get("pairingSecondsRemaining"))
        plugin.command("phone.stopPairing", {})
        time.sleep(0.5)
        check(plugin.command("phone.status").get("pairingOpen") is False, "and closed again")

        health = plugin.rpc("plugin.health", {}).get("result") or {}
        print("  plugin.health: %s" % json.dumps(health)[:300], flush=True)

        plugin.rpc("plugin.shutdown", {})
        check(plugin.proc.wait(15) == 0, "the plugin shut down cleanly")
    except Exception:
        plugin.proc.kill()
        plugin.log.flush()
        print("--- the plugin's own log (last lines)", flush=True)
        with open(os.path.join(data_dir, "plugin-stderr.log"), "r", errors="replace") as log:
            for line in log.readlines()[-40:]:
                print("  " + line.rstrip()[:220], flush=True)
        raise
    if os.environ.get("AOKIE_PLUGIN_CHECK_LOG"):
        print("--- the plugin's own log", flush=True)
        with open(os.path.join(data_dir, "plugin-stderr.log"), "r", errors="replace") as log:
            for line in log.readlines()[-int(os.environ["AOKIE_PLUGIN_CHECK_LOG"]):]:
                print("  " + line.rstrip()[:220], flush=True)
    print("the plugin's radio came up on the stand-in dongle and went down cleanly", flush=True)


if __name__ == "__main__":
    main()
