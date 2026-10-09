//! What every USB transport of the radio has to know about HCI over USB,
//! with no USB call in it: how events come off the interrupt pipe, which
//! alternate setting carries a voice link, and how a SCO packet lies over
//! isochronous frames. Pure code, so it is tested on every system the
//! crate builds for, whichever transport that system uses.

use std::collections::VecDeque;

/// HCI events off a USB interrupt pipe, one whole event at a time.
///
/// The pipe is a byte stream cut into USB packets (16 bytes on a
/// full-speed dongle), not a stream of events. An event longer than one
/// packet arrives in several; an event whose size is a whole multiple of
/// the packet size ends with no short packet, so the next event follows
/// it with nothing in between; and a transfer bigger than one event can
/// hold two. The event's own header says where it ends (`code, length,
/// parameters`), which is what Linux's `btusb` goes by too.
#[derive(Debug, Default)]
pub struct EventStream {
    bytes: VecDeque<u8>,
}

impl EventStream {
    /// Bytes as they came off the pipe, in order.
    pub fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes);
    }

    /// The next whole event, if all of it has arrived.
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let length = *self.bytes.get(1)? as usize;
        let total = 2 + length;
        if self.bytes.len() < total {
            return None;
        }
        Some(self.bytes.drain(..total).collect())
    }

    /// How many bytes of an event not yet whole are waiting.
    pub fn pending(&self) -> usize {
        self.bytes.len()
    }

    /// Forget what is waiting (the pipe was flushed under it).
    pub fn clear(&mut self) {
        self.bytes.clear();
    }
}

/// BTstack USB-transport alt-setting tables. Bit 5 of voice_setting
/// (input sample size) selects which table; the connection count
/// indexes into it. mSBC + transparent + 1 connection → alt 1.
pub const SCO_ALT_SETTINGS_8_BIT: [u8; 3] = [1, 2, 3];
pub const SCO_ALT_SETTINGS_16_BIT: [u8; 3] = [2, 4, 5];

pub fn sco_alt_setting_for_voice(voice_setting: u16, connection_count: usize) -> Option<u8> {
    // BTstack `hci_transport_h2_winusb.c:907-913` picks the alt setting
    // purely from voice_setting bit 5 (input sample size) and the
    // connection count — there is no transparent → alt 6 special case.
    // mSBC over USB rides alt 1 (MPS=9) with 24-byte HCI SCO payloads
    // (see `AOKIE_SCO_USB_PAYLOAD_BYTES`). Routing transparent voice to
    // a separate alt-6 pipe was an over-read of the Core spec USB
    // Transport Layer table: BTstack's `hci.c` keeps the same path for
    // CVSD and mSBC and that is the configuration proven to work on the
    // Broadcom 21ec dongle in our worktree comparison.
    let index = connection_count.checked_sub(1)?;
    if voice_setting & 0x0020 != 0 {
        SCO_ALT_SETTINGS_16_BIT.get(index).copied()
    } else {
        SCO_ALT_SETTINGS_8_BIT.get(index).copied()
    }
}

/// Which alternate setting to switch to for a voice link, given the ones
/// the dongle has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoAltChoice {
    /// The one the tables ask for.
    Asked(u8),
    /// The dongle does not have the one asked for; this one will carry
    /// the link instead.
    Instead { asked: u8, alt: u8 },
    /// The dongle has neither the one asked for nor a stand-in: leave the
    /// interface as it is.
    Nothing { asked: u8 },
}

/// The alternate setting for `asked` among `available`. A dongle that
/// lacks the one asked for gets the first 16-bit setting it does have
/// (the fallback the WinUSB transport has always made: a missing setting
/// must not end the call). An empty `available` means "not known", and
/// the one asked for is tried as it is.
pub fn choose_sco_alt(asked: u8, available: &[u8]) -> ScoAltChoice {
    if available.is_empty() || available.contains(&asked) {
        return ScoAltChoice::Asked(asked);
    }
    match SCO_ALT_SETTINGS_16_BIT
        .iter()
        .copied()
        .find(|alt| available.contains(alt))
    {
        Some(alt) => ScoAltChoice::Instead { asked, alt },
        None => ScoAltChoice::Nothing { asked },
    }
}

/// Whether wide-band speech (mSBC) has a setting to ride: voice setting
/// 0x0043 (transparent, 8-bit input) asks for an 8-bit setting.
pub fn msbc_alt_available(available: &[u8]) -> bool {
    SCO_ALT_SETTINGS_8_BIT
        .iter()
        .any(|alt| available.contains(alt))
}

/// How an HCI SCO packet of `packet_len` bytes lies over the frames of an
/// isochronous OUT pipe whose frames hold `max_packet_size` bytes: whole
/// frames, then one shorter frame for what is left. Writes the lengths
/// into `frames` and returns how many it used; `None` when the packet is
/// empty, the pipe is not up (`max_packet_size` 0) or the packet needs
/// more frames than `frames` has. A packet is never cut short to fit: a
/// truncated voice packet is noise.
pub fn iso_frame_lengths(
    packet_len: usize,
    max_packet_size: usize,
    frames: &mut [u32],
) -> Option<usize> {
    if packet_len == 0 || max_packet_size == 0 {
        return None;
    }
    let count = packet_len.div_ceil(max_packet_size);
    if count > frames.len() {
        return None;
    }
    let mut left = packet_len;
    for frame in frames.iter_mut().take(count) {
        let n = left.min(max_packet_size);
        *frame = n as u32;
        left -= n;
    }
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An event of `length` parameter bytes whose bytes say where they are.
    fn event(code: u8, length: u8) -> Vec<u8> {
        let mut out = vec![code, length];
        out.extend((0..length).map(|i| code ^ i));
        out
    }

    #[test]
    fn one_event_in_one_packet_comes_out_whole() {
        let mut stream = EventStream::default();
        let e = event(0x0e, 4);
        stream.push(&e);
        assert_eq!(stream.pop(), Some(e));
        assert_eq!(stream.pop(), None);
        assert_eq!(stream.pending(), 0);
    }

    #[test]
    fn an_event_twice_the_packet_size_is_not_handed_out_until_it_is_whole() {
        // 32 bytes on a 16-byte pipe: two full packets and no short one.
        let mut stream = EventStream::default();
        let e = event(0x2f, 30);
        assert_eq!(e.len(), 32);
        stream.push(&e[..16]);
        assert_eq!(stream.pop(), None);
        assert_eq!(stream.pending(), 16);
        stream.push(&e[16..]);
        assert_eq!(stream.pop(), Some(e));
        assert_eq!(stream.pop(), None);
    }

    #[test]
    fn two_events_back_to_back_come_out_in_order() {
        // The first ends on a packet boundary, so the second follows it
        // in the same transfer.
        let mut stream = EventStream::default();
        let first = event(0x0f, 14);
        let second = event(0x13, 5);
        assert_eq!(first.len(), 16);
        let mut wire = first.clone();
        wire.extend(&second);
        stream.push(&wire);
        assert_eq!(stream.pop(), Some(first));
        assert_eq!(stream.pop(), Some(second));
        assert_eq!(stream.pop(), None);
    }

    #[test]
    fn a_fragment_waits_across_a_timeout_for_the_rest() {
        // Nothing arrives for a while (the caller's read timed out); the
        // bytes already here must still be here when the rest comes.
        let mut stream = EventStream::default();
        let e = event(0x03, 11);
        stream.push(&e[..1]);
        assert_eq!(stream.pop(), None, "one byte is not even a header");
        stream.push(&e[1..7]);
        assert_eq!(stream.pop(), None);
        assert_eq!(stream.pending(), 7);
        stream.push(&e[7..]);
        assert_eq!(stream.pop(), Some(e));
    }

    #[test]
    fn an_event_with_no_parameters_is_two_bytes() {
        let mut stream = EventStream::default();
        stream.push(&[0x10, 0x00, 0x0e]);
        assert_eq!(stream.pop(), Some(vec![0x10, 0x00]));
        assert_eq!(stream.pop(), None);
        assert_eq!(stream.pending(), 1);
        stream.clear();
        assert_eq!(stream.pending(), 0);
    }

    #[test]
    fn the_alt_setting_tables_are_btstacks() {
        // 16-bit linear CVSD (0x0060) and transparent mSBC (0x0043).
        assert_eq!(sco_alt_setting_for_voice(0x0060, 1), Some(2));
        assert_eq!(sco_alt_setting_for_voice(0x0060, 2), Some(4));
        assert_eq!(sco_alt_setting_for_voice(0x0060, 3), Some(5));
        assert_eq!(sco_alt_setting_for_voice(0x0043, 1), Some(1));
        assert_eq!(sco_alt_setting_for_voice(0x0043, 2), Some(2));
        assert_eq!(sco_alt_setting_for_voice(0x0060, 0), None);
        assert_eq!(sco_alt_setting_for_voice(0x0060, 4), None);
    }

    #[test]
    fn a_dongle_without_the_setting_asked_for_gets_a_16_bit_one() {
        let usual = [1, 2, 3, 4, 5];
        assert_eq!(choose_sco_alt(2, &usual), ScoAltChoice::Asked(2));
        assert_eq!(choose_sco_alt(1, &usual), ScoAltChoice::Asked(1));
        // No setting 1: mSBC's packets ride the first 16-bit setting.
        assert_eq!(
            choose_sco_alt(1, &[2, 4]),
            ScoAltChoice::Instead { asked: 1, alt: 2 }
        );
        assert_eq!(
            choose_sco_alt(2, &[1, 4, 5]),
            ScoAltChoice::Instead { asked: 2, alt: 4 }
        );
        assert_eq!(choose_sco_alt(2, &[1, 3]), ScoAltChoice::Nothing { asked: 2 });
        // Not known: try what was asked.
        assert_eq!(choose_sco_alt(2, &[]), ScoAltChoice::Asked(2));
    }

    #[test]
    fn wide_band_speech_needs_an_8_bit_setting() {
        assert!(msbc_alt_available(&[1, 2, 3, 4, 5]));
        assert!(msbc_alt_available(&[3]));
        assert!(!msbc_alt_available(&[4, 5]));
        assert!(!msbc_alt_available(&[]));
    }

    #[test]
    fn a_voice_packet_lies_over_whole_frames_and_one_shorter() {
        let mut frames = [0u32; 8];
        // CVSD: 48 bytes + 3 of header over 17-byte frames.
        assert_eq!(iso_frame_lengths(51, 17, &mut frames), Some(3));
        assert_eq!(&frames[..3], &[17, 17, 17]);
        // mSBC over setting 1: 24 + 3 over 9-byte frames.
        assert_eq!(iso_frame_lengths(27, 9, &mut frames), Some(3));
        assert_eq!(&frames[..3], &[9, 9, 9]);
        // A last frame that is not full.
        assert_eq!(iso_frame_lengths(63, 17, &mut frames), Some(4));
        assert_eq!(&frames[..4], &[17, 17, 17, 12]);
        assert_eq!(iso_frame_lengths(5, 9, &mut frames), Some(1));
        assert_eq!(frames[0], 5);
    }

    #[test]
    fn a_voice_packet_that_does_not_fit_is_refused_not_cut() {
        let mut frames = [0u32; 3];
        assert_eq!(iso_frame_lengths(63, 9, &mut frames), None);
        assert_eq!(iso_frame_lengths(0, 9, &mut frames), None);
        assert_eq!(iso_frame_lengths(27, 0, &mut frames), None);
    }
}
