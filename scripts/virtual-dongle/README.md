# A stand-in Bluetooth dongle

Aokie reaches its dongle over raw USB: WinUSB on Windows, libusb on Linux and
macOS. The libusb side (`crates/aokie-bluetooth/src/aokie_radio/libusb.rs`)
can be run here with no hardware at all.

`dongle.py` is a USB Bluetooth controller served over USB/IP. Linux's
`vhci-hcd` attaches it as if it had been plugged in, the kernel's own
Bluetooth driver binds it as it would a real one, and
`crates/aokie-bluetooth/tests/virtual_dongle.rs` then drives the transport
against it.

## Run it

On Linux, as root (it loads `vhci-hcd`, attaches a USB device and removes both
again when it is done):

```sh
sudo sh scripts/virtual-dongle/run.sh
```

Anywhere Docker runs, without touching your own system's kernel:

```sh
docker build -t aokie-virtual-dongle scripts/virtual-dongle
docker run --rm --privileged -v /lib/modules:/lib/modules:ro -v "$PWD":/src:ro aokie-virtual-dongle
```

The kernel has to be built with USB/IP (`CONFIG_USBIP_VHCI_HCD`); Docker
Desktop's is. `WITH_BTUSB=0` leaves the kernel's Bluetooth driver out, so
that nothing has to be taken from it. `AOKIE_CARGO_ARGS="--features
rusb/vendored"` builds libusb from the sources `rusb` carries, which is the
libusb a Mac build links.

`WITH_PLUGIN=1` (`docker run -e WITH_PLUGIN=1 ...`) goes one step further:
it builds the plugin and runs it whole against the stand-in
(`plugin_check.py`). The plugin is started as OAIY Desktop starts it, its
own radio thread opens the dongle and brings it up, `dongle.diagnostics`,
`dongle.list` and `phone.status` are answered, a pairing window is opened
and closed, and the plugin shuts down and gives the dongle back. That build
is the heavy one (minutes; it links libwebrtc).

## What it checks

* the dongle is found and described from its descriptors, the voice endpoints
  on the second interface in all six alternate settings;
* it is taken from the system's driver, and given back when the transport
  closes (`run.sh` compares the driver before and after);
* the bring-up commands;
* an event of exactly two USB packets (nothing short ends it) comes whole and
  at once; events back to back ahead of a Command Complete come out in order;
* an event that arrives in three packets over 90 ms, polled every 5 ms as the
  runtime polls, comes whole, and none of the polls that found nothing took a
  transfer back from the dongle (the dongle counts);
* ACL packets of 139, 128, 1025 and 5 bytes come back unchanged from a dongle
  that holds them back;
* a voice link on setting 2 (16-bit CVSD) and one on setting 1 (mSBC): the
  inbound stream arrives in sequence with nothing lost or repeated, every
  outbound byte reaches the dongle in sequence, a packet too big for the
  frames is refused, and ending the link takes the transfers back and sets the
  interface to setting 0;
* closing with a link up, and opening again;
* pulling the dongle out with transfers on every pipe ends the reads with an
  error (not "nothing yet", not a hang) and the close returns;
* with `WITH_PLUGIN=1`: the plugin's radio comes up on the dongle and goes
  down cleanly, as above.

## What it cannot check

It is not a radio: nothing is on the air and no phone can pair with it. Its
1 ms frames are timed by a program's sleep, not by a USB host controller, so
it says nothing about how call audio sounds on real hardware. And it runs
libusb's Linux half; the macOS half of libusb has to be run on a Mac
(`cargo run -p aokie-bluetooth --example dongle_probe`, see
`docs/HARDWARE.md`).
