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

use aokie_bluetooth::aokie_radio::{bmessage, hfp, l2cap, map_listing, map_mns, obex, rfcomm, sdp};
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

// Review 4: the AT and RFCOMM parsers, the MNS server fed a stream, and
// the ACL path with several frames to a packet. Fewer cases each: they
// drive whole state machines, and run in the debug test gate.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The phone's AT result lines, whole and split across frames, applied
    /// to the hands-free state machine (a stateful run per case).
    #[test]
    fn hfp_at_results_never_panic(
        lines in proptest::collection::vec("[+A-Z0-9:,\"() =-]{0,40}", 0..24),
        cuts in proptest::collection::vec(0usize..64, 0..8),
        noise in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let _ = hfp::parse_ag_results(&noise);
        let text = lines.join("\r\n");
        let mut state = hfp::HfpHandsFreeState::new();
        let mut carry = String::new();
        let mut rest = text.as_bytes();
        for cut in cuts.iter().chain(std::iter::once(&usize::MAX)) {
            let mut at = (*cut).min(rest.len());
            while !text.is_char_boundary(text.len() - rest.len() + at) {
                at += 1;
            }
            let (chunk, tail) = rest.split_at(at);
            rest = tail;
            if let Ok(results) = hfp::parse_ag_results_buffered(&mut carry, chunk) {
                for result in &results {
                    let _ = state.apply_result(result);
                }
            }
            prop_assert!(carry.len() <= 1024);
        }
        let _ = state.after_refusal("AT+CHUP");
        let _ = state.after_refusal("ATA");
    }

    /// RFCOMM frames and multiplexer commands, and a server and a client
    /// session each fed a run of packets.
    #[test]
    fn rfcomm_frames_never_panic(
        packets in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..160), 0..12),
    ) {
        let mut server = rfcomm::RfcommState::new();
        let mut client = rfcomm::RfcommClientState::new(3);
        for packet in &packets {
            if let Ok(frame) = rfcomm::parse_frame(packet) {
                let _ = rfcomm::parse_multiplexer_command(frame.payload);
            }
            let _ = rfcomm::parse_multiplexer_command(packet);
            let _ = server.handle_packet(packet);
            let _ = client.handle_packet(packet);
        }
    }

    /// The MNS server fed a stream mixing well-formed requests, pieces of
    /// them and junk. Every reply is one whole OBEX packet.
    #[test]
    fn mns_server_stream_never_panics(
        steps in proptest::collection::vec((0u8..5, proptest::collection::vec(any::<u8>(), 0..96), 0usize..400), 0..24),
    ) {
        let connect = obex::build_connect_request(Some(&map_mns::MNS_TARGET_UUID), 0x1000);
        let report = br#"<MAP-event-report><event type="NewMessage" handle="0001"/></MAP-event-report>"#;
        let mut server = map_mns::MnsServer::new();
        for (kind, junk, cut) in &steps {
            let put = obex::build_put_request(
                0xa04c_1e01,
                vec![obex::Header::obex_type(map_mns::TYPE_EVENT_REPORT)],
                report.to_vec(),
            );
            let bytes: Vec<u8> = match kind {
                0 => connect.clone(),
                1 => put.clone(),
                2 => put[..(*cut).min(put.len())].to_vec(),
                3 => put[(*cut).min(put.len())..].to_vec(),
                _ => junk.clone(),
            };
            for reply in server.feed_bytes(&bytes) {
                prop_assert!(reply.len() >= 3);
                prop_assert_eq!(u16::from_be_bytes([reply[1], reply[2]]) as usize, reply.len());
            }
            let _ = server.take_events();
        }
    }

    /// ACL packets holding several basic frames each (signalling and data,
    /// well-formed and not), some continued in a second packet. Every
    /// reply is a whole ACL packet with a whole basic frame in it.
    #[test]
    fn acl_packets_with_several_frames_never_panic(
        packets in proptest::collection::vec(
            proptest::collection::vec((0u8..4, proptest::collection::vec(any::<u8>(), 0..40)), 1..5),
            1..8,
        ),
        split in 0usize..64,
    ) {
        let mut state = l2cap::L2capState::new();
        for frames in &packets {
            let mut payload = Vec::new();
            for (kind, bytes) in frames {
                let cid = match kind {
                    0 | 1 => l2cap::CID_SIGNALING,
                    2 => 0x0040,
                    _ => u16::from(bytes.first().copied().unwrap_or(0)) << 4,
                };
                let mut body = bytes.clone();
                if *kind == 1 && body.len() >= 4 {
                    // A signalling command with a length that may lie.
                    body[0] &= 0x1f;
                }
                payload.extend(l2cap::build_basic_frame(cid, &body));
            }
            let at = split.min(payload.len());
            let first = l2cap::build_acl_packet(0x000b, l2cap::ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE, 0, &payload[..at]);
            let rest = l2cap::build_acl_packet(0x000b, l2cap::ACL_PACKET_BOUNDARY_CONTINUING_FRAGMENT, 0, &payload[at..]);
            for packet in [first, rest] {
                if let Ok(replies) = state.handle_acl_packet(&packet) {
                    for reply in replies {
                        let acl = l2cap::parse_acl_frame(&reply);
                        prop_assert!(acl.is_ok());
                        prop_assert!(l2cap::parse_basic_frame(acl.unwrap().payload).is_ok());
                    }
                }
            }
        }
    }
}

/// The review 4 probe, as fixed cases. #13: one OBEX header declaring a
/// length under 3 wedged the MNS buffer, so no later CONNECT was answered.
#[test]
fn probe_mns_bad_length_then_connect() {
    let mut mns = map_mns::MnsServer::new();
    assert_eq!(mns.feed_bytes(&[0x80, 0x00, 0x01]).len(), 1);
    let connect = obex::build_connect_request(Some(&map_mns::MNS_TARGET_UUID), 0x1000);
    for _ in 0..3 {
        let replies = mns.feed_bytes(&connect);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0][0], obex::RSP_OK);
        assert_eq!(mns.state(), &map_mns::MnsState::Connected);
    }
}

/// #24: a body assembler with a limit refuses chunks past it.
#[test]
fn probe_body_assembler_has_a_limit() {
    let mut asm = obex::BodyAssembler::with_limit(obex::NOTIFICATION_BODY_LIMIT);
    let chunk = obex::Packet {
        opcode: obex::RSP_CONTINUE,
        fixed_payload: vec![],
        headers: vec![obex::Header::body(vec![0u8; 60000])],
    };
    assert!(asm.push(&chunk).is_ok());
    assert!(asm.push(&chunk).is_err());
    assert!(asm.bytes().len() <= obex::NOTIFICATION_BODY_LIMIT);
    let mut asm = obex::BodyAssembler::with_limit(obex::MESSAGE_BODY_LIMIT);
    let mut refused = false;
    for _ in 0..200 {
        if asm.push(&chunk).is_err() {
            refused = true;
            break;
        }
    }
    assert!(refused, "a message body stops at {} bytes", obex::MESSAGE_BODY_LIMIT);
}

/// #14: an answerable InformationRequest and a truncated command in one
/// ACL packet, in either order: both are answered (the reply and a Command
/// Reject), where the truncated one used to drop the other's reply.
#[test]
fn probe_acl_two_frames_one_bad() {
    use l2cap::*;
    let info_req = [SIGNAL_INFORMATION_REQUEST, 0x01, 0x02, 0x00, 0x02, 0x00];
    let bad = [0x02u8, 0x02, 0x10, 0x00, 0x01];
    let mut state = L2capState::new();
    let one = build_acl_packet(0x0b, ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE, 0, &build_basic_frame(CID_SIGNALING, &info_req));
    assert_eq!(state.handle_acl_packet(&one).map(|v| v.len()), Ok(1));
    for (a, b) in [(&info_req[..], &bad[..]), (&bad[..], &info_req[..])] {
        let mut both = build_basic_frame(CID_SIGNALING, a);
        both.extend(build_basic_frame(CID_SIGNALING, b));
        let packet = build_acl_packet(0x0b, ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE, 0, &both);
        assert_eq!(state.handle_acl_packet(&packet).map(|v| v.len()), Ok(2));
    }
}
