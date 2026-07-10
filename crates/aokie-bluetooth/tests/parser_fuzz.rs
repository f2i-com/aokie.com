//! Property-based fuzzing of the byte-stream parsers (audit AOK-TEST-002).
//!
//! Every parser here consumes bytes that ultimately come from a physical
//! phone or Bluetooth controller — i.e. from OUTSIDE the trust boundary. The
//! invariant under test is the one a hostile or merely-buggy peer must never
//! be able to violate: **no parser panics, and none reads out of bounds, on
//! ARBITRARY input.** Returning `Err`/`None`/an empty result is fine; an
//! unwrap-on-a-short-slice, a slice-index panic, or an unbounded allocation
//! is a remotely-triggerable crash of the receptionist.
//!
//! proptest drives thousands of adversarial byte strings per parser and
//! shrinks any failure to a minimal reproducer. Deterministic and fast, so
//! it runs in the normal `cargo test` gate rather than a separate fuzz job.

use aokie_bluetooth::aokie_radio::{bmessage, l2cap, map_listing, obex, sdp};
use aokie_bluetooth::msbc::{MsbcDecoder, MsbcStreamFramer, MSBC_FRAME_SIZE, MSBC_H2_PACKET_SIZE};
use proptest::prelude::*;

proptest! {
    // A wide length window catches both the truncated-header cases (a length
    // field promising more than the buffer holds) and the large-input paths
    // (unbounded loops / allocations).
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// L2CAP ACL frame parser — the entry point for every ACL byte the
    /// controller delivers.
    #[test]
    fn l2cap_acl_frame_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let _ = l2cap::parse_acl_frame(&bytes);
    }

    /// L2CAP basic frame (the payload inside an ACL frame).
    #[test]
    fn l2cap_basic_frame_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let _ = l2cap::parse_basic_frame(&bytes);
    }

    /// L2CAP signalling commands — a length-prefixed command stream, the
    /// classic place a lying length field wrecks a hand-rolled parser.
    #[test]
    fn l2cap_signaling_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let _ = l2cap::parse_signaling_commands(&bytes);
    }

    /// OBEX header parser (MAP/PBAP transport) — TLV headers with 16-bit
    /// lengths supplied by the peer.
    #[test]
    fn obex_header_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let _ = obex::Header::parse(&bytes);
    }

    /// SDP data element — a recursively-typed structure; malformed nesting
    /// must terminate, never recurse or index off the end.
    #[test]
    fn sdp_data_element_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
        let _ = sdp::parse_data_element(&bytes);
    }

    /// The full SDP server request handler (builds a response from a request
    /// PDU) — exercises the parser plus the response builder.
    #[test]
    fn sdp_handle_request_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
        let server = sdp::SdpServer::new_with_hfp();
        let _ = server.handle_request(&bytes);
    }

    /// MAP bMessage parser — infallible by signature (returns a struct), so
    /// the property is simply that it terminates without panicking on any
    /// bytes (including ones that look like partial headers).
    #[test]
    fn bmessage_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let _ = bmessage::parse(&bytes);
    }

    /// MAP folder/message listing (XML-ish) parser.
    #[test]
    fn map_listing_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let _ = map_listing::parse_listing(&bytes);
    }

    /// mSBC H2 stream framer — fed arbitrary SCO byte chunks, it must find
    /// sync (or not) without ever indexing past its ring buffer. Draining
    /// every frame it yields proves the sync search stays in bounds.
    #[test]
    fn msbc_h2_framer_never_panics(chunks in proptest::collection::vec(
        proptest::collection::vec(any::<u8>(), 0..200), 0..16)) {
        let mut framer = MsbcStreamFramer::new();
        for chunk in &chunks {
            framer.push(chunk);
            while let Some(frame) = framer.next_frame() {
                prop_assert_eq!(frame.len(), MSBC_H2_PACKET_SIZE);
            }
        }
    }

    /// mSBC audio decoder — a fixed-size frame of arbitrary bytes. Bad
    /// header/CRC must return Err, never panic in the synthesis maths.
    #[test]
    fn msbc_decoder_never_panics(frame in proptest::array::uniform(any::<u8>()).prop_map(|a: [u8; MSBC_FRAME_SIZE]| a)) {
        let mut dec = MsbcDecoder::new();
        let _ = dec.decode(&frame);
    }
}
