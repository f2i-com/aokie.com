#!/usr/bin/env python3
"""A stand-in USB Bluetooth dongle, served over USB/IP.

Linux's `vhci-hcd` attaches it as if it were plugged in, so Aokie's libusb
transport (crates/aokie-bluetooth/src/aokie_radio/libusb.rs) can be run
against a device with no hardware: enumeration, taking the device from the
system's own driver, the bring-up commands, events cut into USB packets,
ACL data, the voice interface's alternate settings and isochronous
transfers both ways. `run.sh` beside this file sets it all up in a
container; `tests/virtual_dongle.rs` in the Bluetooth crate is the test.

What it is not: a radio. Nothing is on the air, no phone can pair with it,
and its 1 ms frames are timed by this program's sleep, not by a USB host
controller. It proves the plumbing, not call audio.

The device has the standard Bluetooth layout: interface 0 with the event
(interrupt IN 0x81), ACL IN (bulk 0x82) and ACL OUT (bulk 0x02) endpoints,
and interface 1 with the voice endpoints (isochronous 0x83 / 0x03) in six
alternate settings of 0, 9, 17, 25, 33 and 49 bytes a frame.

Beyond the commands every controller answers it has a few of its own
(vendor commands, OGF 0x3f) that make it misbehave on purpose:

  0xfc01  answer with a Command Complete of exactly 32 bytes: two full USB
          packets and no short one to end them
  0xfc02  three unrelated events back to back, then the Command Complete
  0xfc03  a Command Complete, then a 40-byte event whose first packet comes
          30 ms later and the rest 60 ms after that
  0xfc04  counters: transfers the host took back (unlinked) per endpoint,
          voice bytes received and how many of them were out of sequence
  0xfc05  from now on send every ACL packet back, held for N ms per USB
          packet it would take (parameter: N)

Usage: dongle.py [PORT]   (default 3240, on 127.0.0.1 only)
"""
import collections
import select
import socket
import struct
import sys
import time

USBIP_VERSION = 0x0111
OP_REQ_DEVLIST, OP_REP_DEVLIST = 0x8005, 0x0005
OP_REQ_IMPORT, OP_REP_IMPORT = 0x8003, 0x0003
CMD_SUBMIT, CMD_UNLINK, RET_SUBMIT, RET_UNLINK = 1, 2, 3, 4
DIR_OUT, DIR_IN = 0, 1
ECONNRESET, EPIPE = 104, 32
SPEED_FULL = 2

BUSID = b"1-1"
VID, PID = 0x1209, 0x0001  # pid.codes' test id: no real dongle has it
# 02:A0:C1:E0:00:01, a locally administered address. The test checks it
# before it does anything else, so that it never runs against a real dongle.
BD_ADDR = bytes.fromhex("02a0c1e00001")
EVENT_MPS, ACL_MPS = 16, 64
VOICE_MPS = [0, 9, 17, 25, 33, 49]
# The voice stream's bytes count up modulo a prime, so that a frame lost,
# repeated or shifted shows wherever it happens.
VOICE_MODULUS = 251

EP_EVENT, EP_ACL_IN, EP_ACL_OUT, EP_VOICE_IN, EP_VOICE_OUT = 0x81, 0x82, 0x02, 0x83, 0x03


def device_descriptor():
    return struct.pack("<BBHBBBBHHHBBBB", 18, 1, 0x0200, 0xE0, 0x01, 0x01, 64, VID, PID, 0x0100, 1, 2, 3, 1)


def endpoint(address, attributes, max_packet, interval):
    return struct.pack("<BBBBHB", 7, 5, address, attributes, max_packet, interval)


def interface(number, setting, endpoints):
    return struct.pack("<BBBBBBBBB", 9, 4, number, setting, len(endpoints), 0xE0, 0x01, 0x01, 0) + b"".join(endpoints)


def config_descriptor():
    body = interface(0, 0, [
        endpoint(EP_EVENT, 0x03, EVENT_MPS, 1),
        endpoint(EP_ACL_IN, 0x02, ACL_MPS, 0),
        endpoint(EP_ACL_OUT, 0x02, ACL_MPS, 0),
    ])
    for setting, size in enumerate(VOICE_MPS):
        body += interface(1, setting, [
            endpoint(EP_VOICE_IN, 0x01, size, 1),
            endpoint(EP_VOICE_OUT, 0x01, size, 1),
        ])
    return struct.pack("<BBHBBBBB", 9, 2, 9 + len(body), 2, 1, 0, 0xE0, 50) + body


def string_descriptor(index):
    if index == 0:
        return struct.pack("<BBH", 4, 3, 0x0409)
    text = {1: "Aokie test rig", 2: "Stand-in Bluetooth dongle", 3: "0001"}.get(index)
    if text is None:
        return None
    data = text.encode("utf-16-le")
    return struct.pack("<BB", 2 + len(data), 3) + data


def usbip_device():
    """struct usbip_usb_device, as OP_REP_IMPORT and OP_REP_DEVLIST carry it."""
    return (b"/sys/devices/aokie/virtual-dongle".ljust(256, b"\0") + BUSID.ljust(32, b"\0")
            + struct.pack(">IIIHHHBBBBBB", 1, 1, SPEED_FULL, VID, PID, 0x0100, 0xE0, 0x01, 0x01, 1, 1, 2))


class Urb:
    """A transfer the host has queued and we have not answered yet."""

    def __init__(self, seq, length, frames, due=0.0, data=b""):
        self.seq, self.length, self.frames, self.due, self.data = seq, length, frames, due, data


class Dongle:
    def __init__(self, conn):
        self.conn = conn
        self.alt = {0: 0, 1: 0}
        # What waits to go up each IN pipe: (not before, bytes). The event
        # pipe holds USB packets, the ACL pipe whole transfers.
        self.events = collections.deque()
        self.acl = collections.deque()
        # What the host has queued on each endpoint.
        self.queued = {EP_EVENT: collections.deque(), EP_ACL_IN: collections.deque(),
                       EP_VOICE_IN: collections.deque(), EP_VOICE_OUT: collections.deque()}
        # When the last frame queued on each voice endpoint ends.
        self.frame_clock = {EP_VOICE_IN: 0.0, EP_VOICE_OUT: 0.0}
        self.unlinked = {EP_EVENT: 0, EP_ACL_IN: 0, EP_VOICE_IN: 0, EP_VOICE_OUT: 0}
        self.voice_in_at = 0
        self.voice_out_bytes = 0
        self.voice_out_breaks = 0
        self.voice_out_next = None
        self.acl_echo_gap = None

    # ---- the wire -------------------------------------------------------

    def read(self, count):
        data = b""
        while len(data) < count:
            chunk = self.conn.recv(count - len(data))
            if not chunk:
                raise ConnectionError("the host went away")
            data += chunk
        return data

    def reply(self, seq, status=0, data=b"", actual=None, frames=None):
        """RET_SUBMIT. For an isochronous transfer `frames` is a list of
        (offset, length, actual length, status)."""
        if actual is None:
            actual = len(data)
        header = struct.pack(">IIIIIiiiii8x", RET_SUBMIT, seq, 0, 0, 0, status, actual, 0,
                             len(frames) if frames is not None else 0, 0)
        tail = b"".join(struct.pack(">IIIi", *frame) for frame in frames) if frames else b""
        self.conn.sendall(header + data + tail)

    # ---- what the host sends --------------------------------------------

    def serve(self):
        while True:
            wait = self.next_wake()
            ready, _, _ = select.select([self.conn], [], [], wait)
            if ready:
                self.receive()
            self.service()

    def receive(self):
        command, seq, _devid, direction, ep = struct.unpack(">IIIII", self.read(20))
        body = self.read(28)
        if command == CMD_UNLINK:
            (victim,) = struct.unpack(">I", body[:4])
            self.unlink(seq, victim)
            return
        if command != CMD_SUBMIT:
            raise ConnectionError("unknown USB/IP command %d" % command)
        _flags, length, _start, frame_count, _interval = struct.unpack(">IiiiI", body[:20])
        setup = body[20:28]
        data = self.read(length) if direction == DIR_OUT and length > 0 else b""
        frames = None
        if ep == 3:
            frames = [struct.unpack(">IIIi", self.read(16)) for _ in range(max(frame_count, 0))]
        address = ep | (0x80 if direction == DIR_IN else 0)
        if ep == 0:
            self.control(seq, setup, data, length)
        elif address == EP_ACL_OUT:
            self.reply(seq, actual=len(data))
            if self.acl_echo_gap is not None:
                packets = max(1, -(-len(data) // ACL_MPS))
                self.acl.append((time.monotonic() + self.acl_echo_gap * packets / 1000.0, data))
        elif address in (EP_EVENT, EP_ACL_IN):
            self.queued[address].append(Urb(seq, length, None))
        elif address in (EP_VOICE_IN, EP_VOICE_OUT):
            # One frame a millisecond, one transfer after another: what a
            # host controller's schedule does with a queue of them.
            start = max(time.monotonic(), self.frame_clock[address])
            due = start + len(frames) / 1000.0
            self.frame_clock[address] = due
            self.queued[address].append(Urb(seq, length, frames, due, data))
        else:
            self.reply(seq, status=-EPIPE)

    def unlink(self, seq, victim):
        """The host takes a transfer back. One still queued is dropped and
        never answered; one already answered is left as it is."""
        status = 0
        for address, queue in self.queued.items():
            for urb in queue:
                if urb.seq == victim:
                    queue.remove(urb)
                    self.unlinked[address] += 1
                    status = -ECONNRESET
                    break
            if status:
                break
        self.conn.sendall(struct.pack(">IIIIIi24x", RET_UNLINK, seq, 0, 0, 0, status))

    def control(self, seq, setup, data, length):
        request_type, request, value, index, _wlength = struct.unpack("<BBHHH", setup)
        answer = None
        if request_type == 0x80 and request == 6:  # GET_DESCRIPTOR
            kind, which = value >> 8, value & 0xFF
            if kind == 1:
                answer = device_descriptor()
            elif kind == 2:
                answer = config_descriptor()
            elif kind == 3:
                answer = string_descriptor(which)
        elif request_type == 0x00 and request == 9:  # SET_CONFIGURATION
            answer = b""
        elif request_type == 0x80 and request == 8:  # GET_CONFIGURATION
            answer = b"\x01"
        elif request_type == 0x01 and request == 11:  # SET_INTERFACE
            if index in self.alt and (index == 1 and value < len(VOICE_MPS) or value == 0):
                self.set_interface(index, value)
                answer = b""
        elif request_type == 0x81 and request == 10:  # GET_INTERFACE
            if index in self.alt:
                answer = bytes([self.alt[index]])
        elif request_type in (0x80, 0x81, 0x82) and request == 0:  # GET_STATUS
            answer = b"\x00\x00"
        elif request_type in (0x00, 0x01, 0x02) and request in (1, 3):  # CLEAR / SET_FEATURE
            answer = b""
        elif request_type == 0x20 and request == 0:  # an HCI command
            self.command(data)
            self.reply(seq, actual=len(data))
            return
        if answer is None:
            self.reply(seq, status=-EPIPE)  # a stall: "no such thing"
        elif request_type & 0x80:
            self.reply(seq, data=answer[:length])
        else:
            self.reply(seq, actual=0)

    def set_interface(self, index, value):
        self.alt[index] = value
        if index == 1:
            # A new voice link starts its streams from the beginning.
            self.voice_in_at = 0
            self.voice_out_bytes = 0
            self.voice_out_breaks = 0
            self.voice_out_next = None
            now = time.monotonic()
            self.frame_clock = {EP_VOICE_IN: now, EP_VOICE_OUT: now}

    # ---- the controller -------------------------------------------------

    def event(self, code, params, delay_ms=0, gaps_ms=None):
        """Queue an HCI event as the USB packets it goes up in. `gaps_ms`
        holds each packet after the first back by that many more ms."""
        raw = bytes([code, len(params)]) + params
        at = time.monotonic() + delay_ms / 1000.0
        for number, start in enumerate(range(0, len(raw), EVENT_MPS)):
            if number and gaps_ms:
                at += gaps_ms[min(number - 1, len(gaps_ms) - 1)] / 1000.0
            self.events.append((at, raw[start:start + EVENT_MPS]))

    def command(self, data):
        if len(data) < 3:
            return
        opcode, length = struct.unpack_from("<HB", data)
        params = data[3:3 + length]

        def complete(returned, **timing):
            self.event(0x0E, bytes([1]) + struct.pack("<H", opcode) + returned, **timing)

        if opcode == 0x1001:  # Read Local Version Information
            complete(struct.pack("<BBHBHH", 0, 0x06, 0x1000, 0x06, 0xFFFF, 0x0001))
        elif opcode == 0x1003:  # Read Local Supported Features
            complete(b"\x00" + bytes.fromhex("ffff8ffedbff5b87"))
        elif opcode == 0x1005:  # Read Buffer Size
            complete(struct.pack("<BHBHH", 0, 1021, 64, 8, 1))
        elif opcode == 0x1009:  # Read BD_ADDR (least significant byte first)
            complete(b"\x00" + BD_ADDR[::-1])
        elif opcode == 0xFC01:
            complete(b"\x00" + bytes(range(1, 27)))
        elif opcode == 0xFC02:
            self.event(0x13, bytes([1, 0x01, 0x00, 0x02, 0x00]))
            self.event(0xFF, bytes(range(14)))
            self.event(0x1B, bytes([0x01, 0x00, 0x05]))
            complete(b"\x00")
        elif opcode == 0xFC03:
            complete(b"\x00")
            self.event(0xFF, bytes(range(100, 138)), delay_ms=30, gaps_ms=[60, 0])
        elif opcode == 0xFC04:
            complete(struct.pack("<BIIIIIIB", 0, self.unlinked[EP_EVENT], self.unlinked[EP_ACL_IN],
                                 self.unlinked[EP_VOICE_IN], self.unlinked[EP_VOICE_OUT],
                                 self.voice_out_bytes, self.voice_out_breaks, self.alt[1]))
        elif opcode == 0xFC05:
            self.acl_echo_gap = params[0] if params else 0
            complete(b"\x00")
        else:
            # Reset, Set Event Mask, the Write commands: done, no more to say.
            complete(b"\x00")

    # ---- the clock ------------------------------------------------------

    def next_wake(self):
        """Seconds until something queued on both sides is due (None: wait
        for the host)."""
        times = []
        if self.queued[EP_EVENT] and self.events:
            times.append(self.events[0][0])
        if self.queued[EP_ACL_IN] and self.acl:
            times.append(self.acl[0][0])
        for address in (EP_VOICE_IN, EP_VOICE_OUT):
            if self.queued[address]:
                times.append(self.queued[address][0].due)
        if not times:
            return None
        return max(0.0, min(times) - time.monotonic())

    def service(self):
        now = time.monotonic()
        # Events: packets fill the transfer until a short one ends it or it
        # is full, as on a real bus. A transfer that is neither keeps
        # waiting with what it has: that is why an event a whole number of
        # packets long is held back from a host that asks for more than one
        # packet at a time.
        queue = self.queued[EP_EVENT]
        while queue and self.events and self.events[0][0] <= now:
            urb = queue[0]
            ended = False
            while self.events and self.events[0][0] <= now and not ended:
                at, packet = self.events[0]
                room = urb.length - len(urb.data)
                if len(packet) > room:
                    if urb.data:
                        ended = True  # full enough: the packet is the next transfer's
                        break
                    # The host asked for less than one packet: it gets the
                    # start of it and the rest waits.
                    self.events[0] = (at, packet[room:])
                    urb.data, ended = packet[:room], True
                    break
                self.events.popleft()
                urb.data += packet
                ended = len(packet) < EVENT_MPS or len(urb.data) == urb.length
            if not ended:
                break
            queue.popleft()
            self.reply(urb.seq, data=urb.data)
        # ACL: one transfer each.
        queue = self.queued[EP_ACL_IN]
        while queue and self.acl and self.acl[0][0] <= now:
            urb = queue.popleft()
            at, data = self.acl.popleft()
            if len(data) > urb.length:
                self.acl.appendleft((at, data[urb.length:]))
                data = data[:urb.length]
            self.reply(urb.seq, data=data)
        # Voice in: every frame carries the next bytes of the counting
        # stream (an interface in setting 0 carries none).
        queue = self.queued[EP_VOICE_IN]
        while queue and queue[0].due <= now:
            urb = queue.popleft()
            size = VOICE_MPS[self.alt[1]]
            data, frames = b"", []
            for offset, length, _actual, _status in urb.frames:
                count = min(length, size)
                chunk = bytes((self.voice_in_at + i) % VOICE_MODULUS for i in range(count))
                self.voice_in_at += count
                data += chunk
                frames.append((offset, length, count, 0))
            self.reply(urb.seq, data=data, frames=frames)
        # Voice out: count what came and whether it carried on from the
        # byte before.
        queue = self.queued[EP_VOICE_OUT]
        while queue and queue[0].due <= now:
            urb = queue.popleft()
            frames, total = [], 0
            for offset, length, _actual, _status in urb.frames:
                for byte in urb.data[offset:offset + length]:
                    if self.voice_out_next is not None and byte != self.voice_out_next:
                        self.voice_out_breaks += 1
                    self.voice_out_next = (byte + 1) % VOICE_MODULUS
                    self.voice_out_bytes += 1
                frames.append((offset, length, length, 0))
                total += length
            self.reply(urb.seq, actual=total, frames=frames)


def handshake(conn):
    """The two requests a host makes before transfers start. True once the
    device is imported and the connection carries transfers."""
    header = b""
    while len(header) < 8:
        chunk = conn.recv(8 - len(header))
        if not chunk:
            return False
        header += chunk
    _version, command, _status = struct.unpack(">HHI", header)
    if command == OP_REQ_DEVLIST:
        conn.sendall(struct.pack(">HHII", USBIP_VERSION, OP_REP_DEVLIST, 0, 1) + usbip_device()
                     + bytes([0xE0, 0x01, 0x01, 0]) * 2)
        return False
    if command == OP_REQ_IMPORT:
        busid = b""
        while len(busid) < 32:
            chunk = conn.recv(32 - len(busid))
            if not chunk:
                return False
            busid += chunk
        if busid.rstrip(b"\0") != BUSID:
            conn.sendall(struct.pack(">HHI", USBIP_VERSION, OP_REP_IMPORT, 1))
            return False
        conn.sendall(struct.pack(">HHI", USBIP_VERSION, OP_REP_IMPORT, 0) + usbip_device())
        return True
    return False


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 3240
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", port))
    server.listen(4)
    print("stand-in dongle on 127.0.0.1:%d (bus id %s)" % (port, BUSID.decode()), flush=True)
    while True:
        conn, _ = server.accept()
        conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        try:
            if handshake(conn):
                print("attached", flush=True)
                Dongle(conn).serve()
        except (ConnectionError, OSError) as error:
            print("detached: %s" % error, flush=True)
        finally:
            conn.close()


if __name__ == "__main__":
    main()
