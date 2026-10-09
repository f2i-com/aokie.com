//! libusb's asynchronous transfers, left with libusb across the caller's
//! polls.
//!
//! The radio's runtime polls every pipe with a wait of a few milliseconds
//! so that it keeps up with call audio. libusb's blocking calls cannot do
//! that safely: a blocking read whose wait runs out is cancelled, and rusb
//! then reports the timeout without the bytes that had already arrived. A
//! 139-byte ACL packet on a full-speed dongle is three USB packets; a
//! cancel between them loses the first two for good, and the tail turns up
//! ahead of the next header (the "garbage prefix" the WinUSB transport's
//! `QueuedInRead` was written to end). Here a poll that finds nothing
//! leaves the transfer where it is, and the next poll waits on the same
//! one, so no USB packet is dropped.
//!
//! Voice needs more than that. An isochronous pipe carries one frame every
//! millisecond whether anyone is listening or not: with no transfer queued
//! a frame is simply lost (inbound) or sent empty (outbound). So each
//! direction keeps a ring of transfers with libusb, takes them back in the
//! order they were given and hands them straight out again.
//!
//! Everything `unsafe` about libusb's transfer API lives in [`Transfer`].
//! Its rule is the one the WinUSB transport learned (audit AK-01): memory
//! libusb may still write to is never freed and never reused. A transfer
//! that cannot be shown to have come back is leaked, with its buffer.

use std::os::raw::{c_int, c_void};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use libusb1_sys as ffi;
use libusb1_sys::constants::{
    LIBUSB_ERROR_INTERRUPTED, LIBUSB_ERROR_NO_DEVICE, LIBUSB_TRANSFER_CANCELLED,
    LIBUSB_TRANSFER_COMPLETED, LIBUSB_TRANSFER_NO_DEVICE, LIBUSB_TRANSFER_OVERFLOW,
    LIBUSB_TRANSFER_STALL, LIBUSB_TRANSFER_TIMED_OUT, LIBUSB_TRANSFER_TYPE_ISOCHRONOUS,
};
use rusb::{GlobalContext, UsbContext};

use crate::aokie_radio::usb_hci::iso_frame_lengths;

/// How long a cancelled transfer is waited for before it is given up as
/// lost (and leaked). libusb answers a cancel within a frame or two; a
/// second is only ever used up by a dongle that has hung or gone.
const RETIRE_WAIT: Duration = Duration::from_millis(1000);

/// Transfers kept with libusb on each voice pipe. Sixteen is the WinUSB
/// transport's ring: with three frames to a transfer that is 48 ms of
/// audio queued, longer than one turn of the runtime's loop, so the
/// stream does not run dry between two visits.
const VOICE_RING: usize = 16;
/// Frames in one inbound voice transfer (BTstack's `NUM_ISO_PACKETS`).
pub(super) const VOICE_IN_FRAMES: usize = 3;
/// The most frames one outbound voice packet may take. The largest packet
/// the stack sends is 63 bytes (mSBC's 60 and a 3-byte header), which is
/// seven frames of the smallest setting's 9 bytes.
const VOICE_OUT_FRAMES: usize = 8;
/// How long an inbound read waits for the oldest transfer when none is
/// back yet, so that a caller in a loop does not spin.
const VOICE_IN_WAIT: Duration = Duration::from_millis(5);
/// How long an outbound write waits for room when every transfer is still
/// with libusb.
const VOICE_OUT_WAIT: Duration = Duration::from_millis(100);

/// The text of "nothing arrived within the wait". It carries the words
/// `manager::is_timeout_error` looks for, so every caller treats it as
/// "nothing yet", as it did the blocking reads' timeout.
pub(super) fn nothing_yet(label: &str) -> String {
    format!(
        "libusb timeout ({}: nothing yet; the read stays queued)",
        label
    )
}

fn gone(label: &str) -> String {
    format!(
        "libusb {}: the dongle is gone (unplugged, or taken by another program)",
        label
    )
}

fn status_text(status: c_int) -> &'static str {
    match status {
        LIBUSB_TRANSFER_COMPLETED => "completed",
        LIBUSB_TRANSFER_TIMED_OUT => "timed out",
        LIBUSB_TRANSFER_CANCELLED => "cancelled",
        LIBUSB_TRANSFER_STALL => "the endpoint stalled",
        LIBUSB_TRANSFER_NO_DEVICE => "no device",
        LIBUSB_TRANSFER_OVERFLOW => "the device sent more than was asked for",
        _ => "transfer error",
    }
}

/// libusb's callback for every transfer of ours: it sets the transfer's
/// latch and does nothing else. It runs on whichever thread is inside
/// libusb's event handling at the time.
extern "system" fn given_back(transfer: *mut ffi::libusb_transfer) {
    if transfer.is_null() {
        return;
    }
    // SAFETY: libusb passes the transfer it was given. Its `user_data` is
    // the latch `Transfer::new` allocated, which lives until the transfer
    // is freed (or for ever, if the transfer was leaked).
    unsafe {
        let latch = (*transfer).user_data as *const AtomicI32;
        if !latch.is_null() {
            (*latch).store(1, Ordering::SeqCst);
        }
    }
}

/// How a transfer ended.
#[derive(Debug, Clone, Copy)]
pub(super) struct Outcome {
    pub status: c_int,
    pub moved: usize,
}

/// One libusb transfer with the memory libusb writes to while it has it:
/// the buffer and a latch its callback sets.
pub(super) struct Transfer {
    raw: NonNull<ffi::libusb_transfer>,
    context: *mut ffi::libusb_context,
    /// The bytes read into or written from. A raw allocation rather than
    /// a `Box` field: libusb holds a pointer into it for as long as the
    /// transfer is out, and a `Box` that moved with this struct would
    /// claim to be the only way in.
    buffer: NonNull<u8>,
    capacity: usize,
    /// 0 while libusb has the transfer, 1 once its callback has run.
    latch: NonNull<AtomicI32>,
    /// Isochronous frames the transfer was allocated with (0: it is a
    /// bulk or interrupt transfer).
    frames: usize,
    /// Given to libusb and not yet taken back with [`Transfer::take`].
    out: bool,
    endpoint: u8,
}

// SAFETY: the pointers are to allocations this struct owns (the buffer and
// the latch) or holds for libusb (the transfer), and libusb's transfer
// calls may be made from any thread. Nothing here is tied to the thread
// that made it. The struct is not `Sync`: every method that touches libusb
// state takes `&mut self` or is read-only on the latch, which is atomic.
unsafe impl Send for Transfer {}

impl Transfer {
    /// A transfer for `endpoint` of an open device, with a buffer of
    /// `capacity` bytes. `kind` is libusb's transfer type; `frames` is how
    /// many isochronous frames it may carry (0 for bulk and interrupt).
    ///
    /// The transfer keeps the device handle's raw pointer: it must be
    /// dropped before `handle` is.
    pub(super) fn new(
        handle: &rusb::DeviceHandle<GlobalContext>,
        endpoint: u8,
        kind: u8,
        capacity: usize,
        frames: usize,
    ) -> Result<Self, String> {
        if capacity == 0 || capacity > c_int::MAX as usize || frames > 1024 {
            return Err(format!(
                "libusb transfer for endpoint 0x{:02x}: {} bytes in {} frames is not a size to ask for",
                endpoint, capacity, frames
            ));
        }
        // SAFETY: plain allocation; a null return is handled.
        let raw = NonNull::new(unsafe { ffi::libusb_alloc_transfer(frames as c_int) })
            .ok_or_else(|| "libusb_alloc_transfer: out of memory".to_string())?;
        // SAFETY: `Box::into_raw` never returns null.
        let buffer = unsafe {
            NonNull::new_unchecked(Box::into_raw(vec![0u8; capacity].into_boxed_slice()) as *mut u8)
        };
        let latch = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(AtomicI32::new(0)))) };
        // SAFETY: `raw` is a transfer libusb has just allocated (zeroed)
        // and nobody else knows of. The buffer and the latch outlive it:
        // `Drop` frees them only after the transfer is back and freed.
        unsafe {
            let t = raw.as_ptr();
            (*t).dev_handle = handle.as_raw();
            (*t).flags = 0;
            (*t).endpoint = endpoint;
            (*t).transfer_type = kind;
            // Never: a poll that gives up leaves the transfer with libusb.
            (*t).timeout = 0;
            (*t).status = 0;
            (*t).length = 0;
            (*t).actual_length = 0;
            (*t).callback = given_back;
            (*t).user_data = latch.as_ptr() as *mut c_void;
            (*t).buffer = buffer.as_ptr();
            (*t).num_iso_packets = frames as c_int;
        }
        Ok(Self {
            raw,
            context: handle.context().as_raw(),
            buffer,
            capacity,
            latch,
            frames,
            out: false,
            endpoint,
        })
    }

    pub(super) fn is_out(&self) -> bool {
        self.out
    }

    /// Whether libusb has given the transfer back (its callback has run).
    pub(super) fn is_back(&self) -> bool {
        // SAFETY: the latch lives as long as `self`.
        self.out && unsafe { self.latch.as_ref() }.load(Ordering::SeqCst) != 0
    }

    /// Give the transfer to libusb: `len` bytes of the buffer, and for an
    /// isochronous transfer the length of each frame (they lie one after
    /// another in the buffer).
    pub(super) fn submit(&mut self, len: usize, frame_lens: &[u32]) -> Result<(), String> {
        if self.out {
            return Err(format!(
                "libusb transfer for endpoint 0x{:02x} is still out",
                self.endpoint
            ));
        }
        let framed: usize = frame_lens.iter().map(|&n| n as usize).sum();
        if len > self.capacity
            || frame_lens.len() > self.frames
            || (self.frames > 0 && framed != len)
        {
            return Err(format!(
                "libusb transfer for endpoint 0x{:02x}: {} bytes in {} frames does not fit ({} bytes, {} frames)",
                self.endpoint,
                len,
                frame_lens.len(),
                self.capacity,
                self.frames
            ));
        }
        // SAFETY: the transfer is ours (not out), so libusb is not reading
        // or writing it. The frame descriptors follow the struct in the
        // allocation `libusb_alloc_transfer(self.frames)` made, and
        // `frame_lens.len() <= self.frames`.
        let rc = unsafe {
            self.latch.as_ref().store(0, Ordering::SeqCst);
            let t = self.raw.as_ptr();
            (*t).status = 0;
            (*t).length = len as c_int;
            (*t).actual_length = 0;
            (*t).num_iso_packets = frame_lens.len() as c_int;
            let frames = std::ptr::addr_of_mut!((*t).iso_packet_desc)
                as *mut ffi::libusb_iso_packet_descriptor;
            for (i, &n) in frame_lens.iter().enumerate() {
                let frame = frames.add(i);
                (*frame).length = n;
                (*frame).actual_length = 0;
                (*frame).status = 0;
            }
            ffi::libusb_submit_transfer(t)
        };
        if rc != 0 {
            return Err(if rc == LIBUSB_ERROR_NO_DEVICE {
                gone("submit")
            } else {
                format!(
                    "libusb_submit_transfer (endpoint 0x{:02x}): error {}",
                    self.endpoint, rc
                )
            });
        }
        self.out = true;
        Ok(())
    }

    /// Wait up to `wait` for libusb to give the transfer back, letting
    /// libusb deliver whatever else has finished meanwhile. False when it
    /// is still out (or was never given).
    pub(super) fn wait(&self, wait: Duration) -> bool {
        if !self.out {
            return false;
        }
        let deadline = Instant::now() + wait;
        loop {
            if self.is_back() {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let tv = libc::timeval {
                tv_sec: left.as_secs() as _,
                tv_usec: left.subsec_micros() as _,
            };
            // SAFETY: the context is the open handle's; the latch outlives
            // the call. libusb returns as soon as the latch is set.
            let rc = unsafe {
                ffi::libusb_handle_events_timeout_completed(
                    self.context,
                    &tv,
                    self.latch.as_ref().as_ptr(),
                )
            };
            if left.is_zero() {
                return self.is_back();
            }
            if rc < 0 && rc != LIBUSB_ERROR_INTERRUPTED {
                // libusb could not wait at all: do not spin on it.
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Take a transfer libusb has given back: how it ended. It is ours
    /// again, and [`Transfer::bytes`] and [`Transfer::frame`] read what it
    /// brought.
    pub(super) fn take(&mut self) -> Outcome {
        debug_assert!(self.is_back(), "taking a transfer libusb still has");
        self.out = false;
        // SAFETY: libusb's callback has run, so libusb is done with it.
        unsafe {
            let t = self.raw.as_ptr();
            Outcome {
                status: (*t).status,
                moved: (*t).actual_length.max(0) as usize,
            }
        }
    }

    /// The buffer. Empty while libusb has the transfer: its bytes are not
    /// ours to read then.
    pub(super) fn bytes(&self) -> &[u8] {
        if self.out {
            return &[];
        }
        // SAFETY: an allocation of `capacity` bytes that only we touch
        // while the transfer is not out.
        unsafe { std::slice::from_raw_parts(self.buffer.as_ptr(), self.capacity) }
    }

    /// The buffer, to fill before a submit. Empty while libusb has the
    /// transfer.
    pub(super) fn bytes_mut(&mut self) -> &mut [u8] {
        if self.out {
            return &mut [];
        }
        // SAFETY: as `bytes`, and `&mut self` makes it the only view.
        unsafe { std::slice::from_raw_parts_mut(self.buffer.as_ptr(), self.capacity) }
    }

    /// One frame of an isochronous transfer that has been taken back: its
    /// status and how many bytes it carried.
    pub(super) fn frame(&self, index: usize) -> (c_int, usize) {
        if self.out || index >= self.frames {
            return (LIBUSB_TRANSFER_CANCELLED, 0);
        }
        // SAFETY: the transfer is ours and `index < self.frames`, the
        // number of descriptors it was allocated with.
        unsafe {
            let frames = std::ptr::addr_of!((*self.raw.as_ptr()).iso_packet_desc)
                as *const ffi::libusb_iso_packet_descriptor;
            let frame = frames.add(index);
            ((*frame).status, (*frame).actual_length as usize)
        }
    }

    /// Ask libusb for the transfer back without waiting for it.
    fn cancel(&mut self) {
        if self.out && !self.is_back() {
            // SAFETY: the transfer is out and not freed. An error means
            // it is already on its way back, which is what we want.
            unsafe { ffi::libusb_cancel_transfer(self.raw.as_ptr()) };
        }
    }

    /// Take the transfer back from libusb before it is done. True once it
    /// is ours; false when libusb did not give it back within the wait,
    /// in which case it must not be reused or freed.
    pub(super) fn retire(&mut self) -> bool {
        if !self.out {
            return true;
        }
        self.cancel();
        if !self.wait(RETIRE_WAIT) {
            return false;
        }
        self.out = false;
        true
    }

    /// Clear a stall on the transfer's endpoint (after a transfer ended
    /// with one). Best effort.
    fn clear_halt(&self) {
        // SAFETY: the device handle outlives the transfer (see `new`).
        unsafe {
            let handle = (*self.raw.as_ptr()).dev_handle;
            let _ = ffi::libusb_clear_halt(handle, self.endpoint);
        }
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if !self.retire() {
            // libusb may still write the transfer, its buffer and its
            // latch. Freeing any of them is the use-after-free path; a few
            // hundred bytes lost once, on a teardown that has already gone
            // wrong, is the lesser harm.
            eprintln!(
                "[AokieRadio] libusb did not give back the transfer on endpoint 0x{:02x}: leaking it rather than freeing memory libusb may still write",
                self.endpoint
            );
            return;
        }
        // SAFETY: the transfer is back (or was never out), so nothing but
        // us points at the three allocations any more.
        unsafe {
            ffi::libusb_free_transfer(self.raw.as_ptr());
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                self.buffer.as_ptr(),
                self.capacity,
            )));
            drop(Box::from_raw(self.latch.as_ptr()));
        }
    }
}

/// One IN transfer kept with libusb on the HCI event (interrupt) or ACL
/// (bulk) pipe across polls.
pub(super) struct QueuedIn {
    transfer: Transfer,
    len: usize,
    label: &'static str,
}

impl QueuedIn {
    pub(super) fn new(
        handle: &rusb::DeviceHandle<GlobalContext>,
        endpoint: u8,
        kind: u8,
        len: usize,
        label: &'static str,
    ) -> Result<Self, String> {
        Ok(Self {
            transfer: Transfer::new(handle, endpoint, kind, len, 0)?,
            len,
            label,
        })
    }

    /// What one transfer brought, waiting up to `wait` for it. When
    /// nothing came the error is [`nothing_yet`] and the transfer stays
    /// with libusb for the next poll.
    pub(super) fn poll(&mut self, wait: Duration) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + wait;
        loop {
            if !self.transfer.is_out() {
                self.transfer
                    .submit(self.len, &[])
                    .map_err(|e| format!("{} ({})", e, self.label))?;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if !self.transfer.wait(left) {
                return Err(nothing_yet(self.label));
            }
            let outcome = self.transfer.take();
            let brought = || self.transfer.bytes()[..outcome.moved.min(self.len)].to_vec();
            match outcome.status {
                LIBUSB_TRANSFER_COMPLETED if outcome.moved > 0 => return Ok(brought()),
                // An empty packet (some controllers end a transfer with
                // one): nothing to hand on. Ask again while there is time.
                LIBUSB_TRANSFER_COMPLETED => {
                    if left.is_zero() {
                        return Err(nothing_yet(self.label));
                    }
                }
                // Taken back early (a flush): what had arrived is real.
                LIBUSB_TRANSFER_CANCELLED | LIBUSB_TRANSFER_TIMED_OUT => {
                    return if outcome.moved > 0 {
                        Ok(brought())
                    } else {
                        Err(nothing_yet(self.label))
                    };
                }
                LIBUSB_TRANSFER_NO_DEVICE => return Err(gone(self.label)),
                LIBUSB_TRANSFER_STALL => {
                    self.transfer.clear_halt();
                    return Err(format!("libusb {}: the endpoint stalled", self.label));
                }
                other => {
                    return Err(format!(
                        "libusb {}: the transfer failed ({})",
                        self.label,
                        status_text(other)
                    ))
                }
            }
        }
    }

    /// Take the read back from libusb and forget what it held (the pipe
    /// is being flushed under it).
    pub(super) fn cancel(&mut self) {
        let _ = self.transfer.retire();
    }
}

/// Ask for every transfer of a ring back at once, then wait for each.
/// True when all of them are ours again.
fn retire_all(slots: &mut [Transfer]) -> bool {
    for slot in slots.iter_mut() {
        slot.cancel();
    }
    let mut all = true;
    for slot in slots.iter_mut() {
        all &= slot.retire();
    }
    all
}

/// The inbound voice pipe: a ring of isochronous transfers kept with
/// libusb, read in the order they were given.
pub(super) struct IsoIn {
    slots: Vec<Transfer>,
    /// The slot libusb will give back next.
    oldest: usize,
    max_packet_size: usize,
    frame_lens: [u32; VOICE_IN_FRAMES],
    /// Resubmits that failed, so that a broken pipe is said once in a
    /// while and not per frame.
    resubmit_failures: u64,
}

impl IsoIn {
    pub(super) fn new(
        handle: &rusb::DeviceHandle<GlobalContext>,
        endpoint: u8,
        max_packet_size: u16,
    ) -> Result<Self, String> {
        let mps = max_packet_size as usize;
        if mps == 0 {
            return Err(format!(
                "libusb SCO in endpoint 0x{:02x} carries no bytes in this alternate setting",
                endpoint
            ));
        }
        let mut slots = Vec::with_capacity(VOICE_RING);
        for _ in 0..VOICE_RING {
            slots.push(Transfer::new(
                handle,
                endpoint,
                LIBUSB_TRANSFER_TYPE_ISOCHRONOUS,
                mps * VOICE_IN_FRAMES,
                VOICE_IN_FRAMES,
            )?);
        }
        Ok(Self {
            slots,
            oldest: 0,
            max_packet_size: mps,
            frame_lens: [max_packet_size as u32; VOICE_IN_FRAMES],
            resubmit_failures: 0,
        })
    }

    /// The bytes of every transfer that is back, in order (possibly none).
    /// The first read gives the whole ring to libusb.
    pub(super) fn read(&mut self) -> Result<Vec<u8>, String> {
        let ring = self.slots.len();
        let len = self.max_packet_size * VOICE_IN_FRAMES;
        if !self.slots.iter().any(Transfer::is_out) {
            // The start of the stream, or a start over after every
            // transfer failed to go back out.
            self.oldest = 0;
            let mut last_error = None;
            for slot in &mut self.slots {
                if let Err(e) = slot.submit(len, &self.frame_lens) {
                    last_error = Some(e);
                }
            }
            if !self.slots.iter().any(Transfer::is_out) {
                return Err(last_error
                    .unwrap_or_else(|| "libusb SCO in: no transfer was accepted".to_string()));
            }
        }

        let mut data = Vec::new();
        let mut first = true;
        // A pipe that hands every transfer straight back (a dongle that
        // fails them at once) must not hold the caller here for ever: the
        // runtime's loop has a control channel and the other pipes to see
        // to. Four turns of the ring is ample for a real burst.
        for _ in 0..ring * 4 {
            let Some(index) = (0..ring)
                .map(|step| (self.oldest + step) % ring)
                .find(|&i| self.slots[i].is_out())
            else {
                break;
            };
            let wait = if first { VOICE_IN_WAIT } else { Duration::ZERO };
            if !self.slots[index].wait(wait) {
                break;
            }
            first = false;
            let outcome = self.slots[index].take();
            if outcome.status == LIBUSB_TRANSFER_NO_DEVICE {
                return Err(gone("SCO in"));
            }
            let bytes = self.slots[index].bytes();
            for frame in 0..VOICE_IN_FRAMES {
                let (status, moved) = self.slots[index].frame(frame);
                // A frame the controller had nothing for, or one that
                // went wrong, is a gap the voice path already covers.
                if status != LIBUSB_TRANSFER_COMPLETED || moved == 0 {
                    continue;
                }
                let at = frame * self.max_packet_size;
                if let Some(chunk) = bytes.get(at..at + moved.min(self.max_packet_size)) {
                    data.extend_from_slice(chunk);
                }
            }
            self.oldest = (index + 1) % ring;
            if let Err(e) = self.slots[index].submit(len, &self.frame_lens) {
                self.resubmit_failures += 1;
                if self.resubmit_failures.is_power_of_two() {
                    eprintln!(
                        "[AokieRadio] libusb SCO in: a transfer did not go back out ({} so far): {}",
                        self.resubmit_failures, e
                    );
                }
            }
        }
        Ok(data)
    }
}

impl Drop for IsoIn {
    fn drop(&mut self) {
        // All at once, so that a ring is taken back in one frame's time
        // and not one transfer after another. Each transfer's own drop
        // then frees it, or leaks it if libusb never gave it back.
        retire_all(&mut self.slots);
    }
}

/// The outbound voice pipe: a ring of isochronous transfers, each one HCI
/// SCO packet, given to libusb as they are written and taken back as they
/// finish. A full ring is the pacing: the write waits for the oldest.
pub(super) struct IsoOut {
    slots: Vec<Transfer>,
    /// The slot the next packet goes into.
    next: usize,
    /// The slot libusb will give back next.
    oldest: usize,
    pending: usize,
    max_packet_size: usize,
}

impl IsoOut {
    pub(super) fn new(
        handle: &rusb::DeviceHandle<GlobalContext>,
        endpoint: u8,
        max_packet_size: u16,
    ) -> Result<Self, String> {
        let mps = max_packet_size as usize;
        if mps == 0 {
            return Err(format!(
                "libusb SCO out endpoint 0x{:02x} carries no bytes in this alternate setting",
                endpoint
            ));
        }
        let mut slots = Vec::with_capacity(VOICE_RING);
        for _ in 0..VOICE_RING {
            slots.push(Transfer::new(
                handle,
                endpoint,
                LIBUSB_TRANSFER_TYPE_ISOCHRONOUS,
                mps * VOICE_OUT_FRAMES,
                VOICE_OUT_FRAMES,
            )?);
        }
        Ok(Self {
            slots,
            next: 0,
            oldest: 0,
            pending: 0,
            max_packet_size: mps,
        })
    }

    /// Queue one HCI SCO packet. It is sent whole or refused: a packet
    /// cut to fit would be noise in the caller's ear.
    pub(super) fn write(&mut self, packet: &[u8]) -> Result<(), String> {
        let mut frame_lens = [0u32; VOICE_OUT_FRAMES];
        let frames = iso_frame_lengths(packet.len(), self.max_packet_size, &mut frame_lens)
            .ok_or_else(|| {
                format!(
                    "HCI SCO packet of {} bytes does not fit {} isochronous frames of {} bytes",
                    packet.len(),
                    VOICE_OUT_FRAMES,
                    self.max_packet_size
                )
            })?;
        self.reap(false)?;
        if self.pending >= self.slots.len() {
            self.reap(true)?;
        }
        let index = self.next;
        let slot = &mut self.slots[index];
        if slot.is_out() {
            return Err(format!(
                "SCO out ring slot {} still in use after drain (pending {})",
                index, self.pending
            ));
        }
        slot.bytes_mut()[..packet.len()].copy_from_slice(packet);
        slot.submit(packet.len(), &frame_lens[..frames])?;
        self.pending += 1;
        self.next = (index + 1) % self.slots.len();
        Ok(())
    }

    /// Take back, oldest first, the transfers libusb is done with. With
    /// `block`, wait for the oldest (the ring is full and a packet needs
    /// its place).
    fn reap(&mut self, mut block: bool) -> Result<(), String> {
        let ring = self.slots.len();
        while self.pending > 0 {
            let index = self.oldest;
            if !self.slots[index].is_out() {
                self.oldest = (index + 1) % ring;
                continue;
            }
            let wait = if block { VOICE_OUT_WAIT } else { Duration::ZERO };
            if self.slots[index].wait(wait) {
                let outcome = self.slots[index].take();
                self.pending -= 1;
                self.oldest = (index + 1) % ring;
                if outcome.status == LIBUSB_TRANSFER_NO_DEVICE {
                    return Err(gone("SCO out"));
                }
                block = false;
            } else if block {
                // The pipe is not taking audio. Take the transfer back; if
                // even that fails its memory may still be written, so the
                // slot is never used again and the stream stops here.
                if !self.slots[index].retire() {
                    return Err(format!(
                        "SCO out slot {} could not be cancelled — stream halted pending teardown",
                        index
                    ));
                }
                self.pending -= 1;
                self.oldest = (index + 1) % ring;
                block = false;
            } else {
                break;
            }
        }
        Ok(())
    }
}

impl Drop for IsoOut {
    fn drop(&mut self) {
        retire_all(&mut self.slots);
    }
}
