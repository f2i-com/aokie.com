#!/bin/sh
# run.sh: attach the stand-in dongle (dongle.py) and run the libusb transport's test against it.
#
# Needs Linux, root, the kernel's `vhci-hcd` module (USB/IP), the `usbip` tool, python3 and cargo. On a computer
# that is not Linux, or to leave your own kernel alone, run it in the container of the Dockerfile beside it:
#
#     docker build -t aokie-virtual-dongle scripts/virtual-dongle
#     docker run --rm --privileged -v /lib/modules:/lib/modules:ro -v "$PWD":/src:ro aokie-virtual-dongle
#
# What it does, in order: loads vhci-hcd, starts the stand-in on 127.0.0.1, attaches it (the kernel then sees a USB
# Bluetooth controller), lets the kernel's own Bluetooth driver bind it (WITH_BTUSB=0 to skip: then nothing has to
# be taken from a system driver), runs `dongle_probe`, the transport test and the unplug test, checks that the
# system's driver got the dongle back, and undoes everything it did. Nothing is on the air.
#
# WITH_PLUGIN=1 also builds the plugin and runs it whole against the stand-in (plugin_check.py): started as OAIY
# Desktop starts it, its radio up on the dongle, its setup commands answered, shut down, the dongle given back.
# That build is the heavy one (it links libwebrtc, which on Linux asks for clang 21: the Dockerfile has it).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
port=${AOKIE_VIRTUAL_DONGLE_PORT:-3240}
with_btusb=${WITH_BTUSB:-1}

[ "$(id -u)" = 0 ] || { echo "run.sh needs root: it loads a kernel module and attaches a USB device"; exit 1; }
for tool in modprobe usbip python3 cargo; do
  command -v "$tool" > /dev/null || { echo "run.sh needs $tool"; exit 1; }
done

# The modules a run may bring in (the two asked for and what they pull in), in the order to unload them. Only those
# that were not loaded before the run are unloaded after it.
modules="btusb btrtl btintel btbcm btmtk bluetooth vhci-hcd usbip-core"
is_loaded() { [ -d "/sys/module/$(echo "$1" | tr - _)" ]; }
were_loaded=""
for module in $modules; do if is_loaded "$module"; then were_loaded="$were_loaded $module"; fi; done

server=""; attached=""; made_node=""
cleanup() {
  status=$?
  [ -n "$attached" ] && usbip detach -p "$attached" > /dev/null 2>&1 || true
  [ -n "$server" ] && kill "$server" 2> /dev/null || true
  [ -n "$made_node" ] && rm -f "$made_node" || true
  # The kernel takes a moment to let go of a device that has just gone: a second pass gets what the first could not.
  for pass in 1 2 3; do
    sleep 0.5
    for module in $modules; do
      case " $were_loaded " in *" $module "*) continue ;; esac
      if is_loaded "$module"; then modprobe -r "$module" 2> /dev/null || true; fi
    done
  done
  exit $status
}
trap cleanup EXIT

load() { # MODULE
  is_loaded "$1" || modprobe "$1" || { echo "the kernel has no $1 module (USB/IP needs CONFIG_USBIP_VHCI_HCD)"; exit 1; }
}

find_dongle() { # print the sysfs folder of the stand-in (1209:0001)
  for device in /sys/bus/usb/devices/*; do
    [ -f "$device/idVendor" ] || continue
    if [ "$(cat "$device/idVendor")" = 1209 ] && [ "$(cat "$device/idProduct")" = 0001 ]; then
      echo "$device"; return 0
    fi
  done
  return 1
}

driver_of() { # INTERFACE FOLDER: the kernel driver bound to it, or "none"
  if [ -e "$1/driver" ]; then basename "$(readlink "$1/driver")"; else echo none; fi
}

if find_dongle > /dev/null; then echo "a stand-in dongle is already attached: detach it first (usbip port, usbip detach -p N)"; exit 1; fi

load vhci-hcd
python3 "$here/dongle.py" "$port" &
server=$!
sleep 0.5
kill -0 "$server" 2> /dev/null || { echo "the stand-in did not start (is port $port taken?)"; exit 1; }

usbip --tcp-port "$port" attach -r 127.0.0.1 -b 1-1
tries=0
until sys=$(find_dongle); do
  tries=$((tries + 1)); [ "$tries" -lt 50 ] || { echo "the kernel did not take the stand-in"; exit 1; }
  sleep 0.1
done
attached=$(usbip port 2> /dev/null | sed -n 's/^Port \([0-9][0-9]*\):.*/\1/p' | head -1 | sed 's/^0*\([0-9]\)/\1/')
bus=$(cat "$sys/busnum"); dev=$(cat "$sys/devnum")
echo "attached as $(basename "$sys"): bus $bus, address $dev, vhci port ${attached:-?}"

# A container's /dev is a snapshot taken when it started: the node of a device that appears later is not in it.
node=$(printf '/dev/bus/usb/%03d/%03d' "$bus" "$dev")
if [ ! -e "$node" ]; then
  mkdir -p "$(dirname "$node")"
  mknod "$node" c 189 $(( (bus - 1) * 128 + dev - 1 ))
  made_node=$node
fi

before=none
if [ "$with_btusb" = 1 ]; then
  load btusb
  tries=0
  until [ "$(driver_of "$sys:1.0")" = btusb ]; do
    tries=$((tries + 1)); [ "$tries" -lt 50 ] || break
    sleep 0.1
  done
  before=$(driver_of "$sys:1.0")
  # The kernel's Bluetooth brings the controller up as far as the stand-in's few answers let it; let it finish.
  sleep 1
else
  # A system may bind its own driver without being asked (a rule that loads btusb for any Bluetooth controller).
  # Give it a moment to, then take the dongle from it, so that this run really has no system driver on the dongle.
  sleep 1
  for interface in "$sys:1.0" "$sys:1.1"; do
    driver=$(driver_of "$interface")
    if [ "$driver" != none ]; then
      basename "$interface" > "/sys/bus/usb/drivers/$driver/unbind" 2> /dev/null || true
    fi
  done
  before=$(driver_of "$sys:1.0")
fi
echo "the system's driver on the dongle before the test: $before (voice interface: $(driver_of "$sys:1.1"))"

export AOKIE_VIRTUAL_DONGLE="usb:$bus:$dev"
cd "$repo"

# AOKIE_CARGO_ARGS: more for cargo, e.g. "--features rusb/vendored" to run against libusb built from the sources
# rusb carries (the libusb a Mac build uses) rather than the system's.
echo "=== dongle_probe"
cargo run -q -p aokie-bluetooth ${AOKIE_CARGO_ARGS:-} --example dongle_probe

echo "=== the transport test"
cargo test -q -p aokie-bluetooth ${AOKIE_CARGO_ARGS:-} --test virtual_dongle the_libusb_transport -- --ignored --nocapture

after=$(driver_of "$sys:1.0")
echo "the system's driver on the dongle after the test: $after (voice interface: $(driver_of "$sys:1.1"))"
if [ "$before" != "$after" ]; then
  echo "FAILED: the system's driver ($before) was not given the dongle back (now: $after)"
  exit 1
fi

if [ "${WITH_PLUGIN:-0}" = 1 ]; then
  echo "=== the whole plugin"
  cargo build -q -p aokie-plugin
  # Linux has no sealing for the outbox's payloads, so a real outbox there refuses them; the check accepts the
  # plaintext trade-off in so many words (outbox.rs), as a developer would. A Mac seals with its Keychain.
  AOKIE_ALLOW_UNPROTECTED_OUTBOX=1 python3 "$here/plugin_check.py" "${CARGO_TARGET_DIR:-$repo/target}/debug/aokie-plugin"
  sleep 1
  after=$(driver_of "$sys:1.0")
  if [ "$before" != "$after" ]; then
    echo "FAILED: the plugin did not give the dongle back to the system's driver ($before, now: $after)"
    exit 1
  fi
fi

echo "=== the unplug test"
AOKIE_VIRTUAL_DONGLE_DETACH="/sys/devices/platform/vhci_hcd.0/detach=${attached:-0}" \
  cargo test -q -p aokie-bluetooth ${AOKIE_CARGO_ARGS:-} --test virtual_dongle pulling_the_stand_in_out -- --ignored --nocapture
attached=""

echo "=== all passed"
