//! SDP client — discover services on the AG (phone).
//!
//! Phase 2a of the MAP/PBAP plan. The existing `sdp.rs` is the SDP
//! *server* — it answers the phone's queries about our HFP record.
//! When we want to act as a client (PBAP, MAP) we have to ask the
//! phone where its services live, since the RFCOMM server channel
//! isn't fixed across vendors.
//!
//! Two responsibilities:
//!   1. Build a `ServiceSearchAttributeRequest` for a given
//!      service-class UUID (e.g. PBAP PSE = 0x112F, MAP MAS = 0x1132).
//!   2. Parse the `ServiceSearchAttributeResponse` and extract the
//!      RFCOMM server channel out of the record's
//!      `ProtocolDescriptorList`.
//!
//! Out of scope:
//!   - Continuation state (multi-packet SDP responses). PBAP/MAP records
//!     are a few hundred bytes; a single 0xffff-byte SDP request always
//!     gets a single response. If we ever target a phone that splits,
//!     this module becomes the right place to add it.
//!   - 32-bit and 128-bit UUIDs in the search pattern. Modern records
//!     use Uuid16 for service classes; we surface a clear error if the
//!     phone replies with anything else.

#![allow(dead_code)] // Phase 2a — runtime integration follows.

use super::sdp::{
    encode_data_element, parse_data_element, DataElement, ParsedDataElement,
    ATTR_PROTOCOL_DESCRIPTOR_LIST, PDU_SERVICE_SEARCH_ATTRIBUTE_REQUEST,
    PDU_SERVICE_SEARCH_ATTRIBUTE_RESPONSE, UUID_RFCOMM,
};

// =============================================================================
// Service-class UUIDs we'll query for.
// =============================================================================

/// Phonebook Access — Phone Server Equipment (the role the phone plays).
pub const UUID_PBAP_PSE: u16 = 0x112F;
/// MAP — Message Access Server (the role the phone plays).
pub const UUID_MAP_MAS: u16 = 0x1132;
/// MAP — Message Notification Server (the role *we* play; querying for
/// it helps confirm whether a phone supports inbound notifications).
pub const UUID_MAP_MNS: u16 = 0x1133;

// =============================================================================
// Request builder.
// =============================================================================

/// Build a `ServiceSearchAttributeRequest` for the given service-class
/// UUID. `max_attribute_bytes` is what we'll accept inbound; `0xffff`
/// is the spec maximum and the only sane choice for our use cases.
///
/// `transaction_id` should be a fresh value the caller picks per query
/// — the AG echoes it back so we can match responses to requests.
///
/// The request asks for **all** attributes (range 0x0000..=0xffff) so
/// we get the ProtocolDescriptorList back. Phones don't charge per
/// byte; sending the full record is simpler than tracking which attrs
/// each profile needs.
pub fn build_service_search_attribute_request(
    transaction_id: u16,
    service_class_uuid: u16,
    max_attribute_bytes: u16,
) -> Vec<u8> {
    let mut search_pattern = Vec::new();
    encode_data_element(DataElement::Uuid16(service_class_uuid), &mut search_pattern);

    let mut attr_id_list = Vec::new();
    encode_data_element(DataElement::Uint32(0x0000_FFFF), &mut attr_id_list);

    let mut params = Vec::with_capacity(32);
    encode_data_element(DataElement::Sequence(&search_pattern), &mut params);
    params.extend_from_slice(&max_attribute_bytes.to_be_bytes());
    encode_data_element(DataElement::Sequence(&attr_id_list), &mut params);
    // Continuation state — empty: this is a fresh request.
    params.push(0);

    let mut pdu = Vec::with_capacity(5 + params.len());
    pdu.push(PDU_SERVICE_SEARCH_ATTRIBUTE_REQUEST);
    pdu.extend_from_slice(&transaction_id.to_be_bytes());
    pdu.extend_from_slice(&(params.len() as u16).to_be_bytes());
    pdu.extend_from_slice(&params);
    pdu
}

// =============================================================================
// Response parsing + RFCOMM channel extraction.
// =============================================================================

/// Parsed view of a `ServiceSearchAttributeResponse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSearchAttributeResponse<'a> {
    pub transaction_id: u16,
    /// The full AttributeLists value — a SequenceOfSequences-of-attrs.
    /// Use `iter_records` to walk it.
    pub attribute_lists: &'a [u8],
    /// Server-supplied continuation. Non-empty means there's more to
    /// come; we don't drive multi-packet today (see module comment).
    pub continuation_state: &'a [u8],
}

/// Parse the server's `ServiceSearchAttributeResponse` PDU. Doesn't
/// allocate — the returned slices borrow `packet`.
pub fn parse_service_search_attribute_response(
    packet: &[u8],
) -> Result<ServiceSearchAttributeResponse<'_>, String> {
    require_len(packet, 5, "SDP response PDU header")?;
    if packet[0] != PDU_SERVICE_SEARCH_ATTRIBUTE_RESPONSE {
        return Err(format!(
            "expected SDP ServiceSearchAttributeResponse 0x07, got 0x{:02x}",
            packet[0]
        ));
    }
    let transaction_id = u16::from_be_bytes([packet[1], packet[2]]);
    let param_len = u16::from_be_bytes([packet[3], packet[4]]) as usize;
    require_len(packet, 5 + param_len, "SDP response parameters")?;
    let params = &packet[5..5 + param_len];

    require_len(params, 2, "SDP AttributeListsByteCount")?;
    let attr_lists_len = u16::from_be_bytes([params[0], params[1]]) as usize;
    if 2 + attr_lists_len > params.len() {
        return Err(format!(
            "SDP AttributeListsByteCount {} exceeds parameters {}B",
            attr_lists_len,
            params.len() - 2
        ));
    }
    let attribute_lists = &params[2..2 + attr_lists_len];

    let continuation = &params[2 + attr_lists_len..];
    if continuation.is_empty() {
        return Err("SDP response missing continuation state byte".to_string());
    }
    let continuation_len = continuation[0] as usize;
    if 1 + continuation_len > continuation.len() {
        return Err(format!(
            "SDP continuation state declared {}B but only {}B remain",
            continuation_len,
            continuation.len() - 1
        ));
    }
    let continuation_state = &continuation[1..1 + continuation_len];

    Ok(ServiceSearchAttributeResponse {
        transaction_id,
        attribute_lists,
        continuation_state,
    })
}

/// Walk the `AttributeLists` sequence and yield each matching record's
/// inner attribute-pair sequence. Each yielded slice is the bytes
/// *inside* the inner sequence (i.e., the `(AttrId, AttrVal)*` payload,
/// without the enclosing Sequence descriptor).
pub fn iter_records(attribute_lists: &[u8]) -> Result<Vec<&[u8]>, String> {
    // The outer sequence wraps the per-record inner sequences.
    let (outer, used) = parse_data_element(attribute_lists)?;
    if used != attribute_lists.len() {
        return Err(format!(
            "SDP AttributeLists has {} trailing bytes after the outer sequence",
            attribute_lists.len() - used
        ));
    }
    let ParsedDataElement::Sequence(outer_payload) = outer else {
        return Err("SDP AttributeLists is not a sequence".to_string());
    };
    let mut records = Vec::new();
    let mut offset = 0;
    while offset < outer_payload.len() {
        let (inner, inner_used) = parse_data_element(&outer_payload[offset..])?;
        let ParsedDataElement::Sequence(inner_payload) = inner else {
            return Err("SDP record root is not a sequence".to_string());
        };
        records.push(inner_payload);
        offset += inner_used;
    }
    Ok(records)
}

/// Find the value of a specific attribute in a record. Returns the raw
/// bytes of the attribute *value* (not its preceding Uint16 AttrId).
pub fn find_attribute<'a>(
    record_payload: &'a [u8],
    attr_id: u16,
) -> Result<Option<&'a [u8]>, String> {
    let mut offset = 0;
    while offset < record_payload.len() {
        let (id_elem, id_used) = parse_data_element(&record_payload[offset..])?;
        let ParsedDataElement::Uint16(id) = id_elem else {
            return Err(format!(
                "SDP record attribute id is not Uint16 (got {:?})",
                id_elem
            ));
        };
        offset += id_used;
        let value_start = offset;
        let (_value_elem, value_used) = parse_data_element(&record_payload[offset..])?;
        offset += value_used;
        if id == attr_id {
            return Ok(Some(&record_payload[value_start..value_start + value_used]));
        }
    }
    Ok(None)
}

/// Extract the RFCOMM server channel from a record's
/// `ProtocolDescriptorList` (attribute 0x0004). Returns `None` if no
/// RFCOMM protocol descriptor is present (some profiles use OBEX over
/// L2CAP instead, in which case the caller wants
/// `extract_l2cap_psm` — left for a future module). Returns `Err`
/// only if the descriptor list is structurally malformed.
pub fn extract_rfcomm_channel(record_payload: &[u8]) -> Result<Option<u8>, String> {
    let Some(pdl_value) = find_attribute(record_payload, ATTR_PROTOCOL_DESCRIPTOR_LIST)? else {
        return Ok(None);
    };
    // pdl_value is a DataElement (a Sequence of Sequences). Parse the
    // outer wrapper first.
    let (outer, used) = parse_data_element(pdl_value)?;
    if used != pdl_value.len() {
        return Err("SDP ProtocolDescriptorList has trailing bytes".to_string());
    }
    let ParsedDataElement::Sequence(outer_payload) = outer else {
        return Err("SDP ProtocolDescriptorList outer is not a sequence".to_string());
    };

    let mut offset = 0;
    while offset < outer_payload.len() {
        let (proto_seq, proto_used) = parse_data_element(&outer_payload[offset..])?;
        let ParsedDataElement::Sequence(proto_payload) = proto_seq else {
            return Err("SDP ProtocolDescriptorList entry is not a sequence".to_string());
        };
        offset += proto_used;

        // First element is the protocol UUID. Second (if present) is
        // its parameter — for RFCOMM the channel; for L2CAP the PSM
        // (which we ignore here).
        let (uuid_elem, uuid_used) = parse_data_element(proto_payload)?;
        let ParsedDataElement::Uuid16(uuid) = uuid_elem else {
            // Some phones encode RFCOMM's UUID as a Uint16 (descriptor
            // 0x09) rather than Uuid16 (descriptor 0x19). Tolerate the
            // alternate encoding so we don't fail on real-world input.
            if let ParsedDataElement::Uint16(uuid) = uuid_elem {
                if uuid == UUID_RFCOMM {
                    let after_uuid = &proto_payload[uuid_used..];
                    if let Ok((channel_elem, _)) = parse_data_element(after_uuid) {
                        if let ParsedDataElement::Uint8(ch) = channel_elem {
                            return Ok(Some(ch));
                        }
                    }
                }
            }
            continue;
        };

        if uuid == UUID_RFCOMM {
            let after_uuid = &proto_payload[uuid_used..];
            if after_uuid.is_empty() {
                return Err("RFCOMM descriptor missing server-channel byte".to_string());
            }
            let (channel_elem, _) = parse_data_element(after_uuid)?;
            let channel = match channel_elem {
                ParsedDataElement::Uint8(ch) => ch,
                // Spec says it's Uint8, but tolerate Uint16 just in case.
                ParsedDataElement::Uint16(ch) if ch <= u8::MAX as u16 => ch as u8,
                other => return Err(format!("RFCOMM channel is not Uint8 (got {:?})", other)),
            };
            return Ok(Some(channel));
        }

        // Skip non-RFCOMM descriptors (L2CAP, OBEX, etc.) — we just
        // wanted the RFCOMM one.
        let _ = uuid;
    }

    Ok(None)
}

// =============================================================================
// Internal utilities.
// =============================================================================

fn require_len(input: &[u8], min_len: usize, name: &str) -> Result<(), String> {
    if input.len() < min_len {
        return Err(format!(
            "{} is too short: expected at least {} bytes, got {}",
            name,
            min_len,
            input.len()
        ));
    }
    Ok(())
}

// =============================================================================
// Tests.
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aokie_radio::sdp::{
        aokie_hfp_service_record, service_search_attribute_response, SdpServer,
        AOKIE_RFCOMM_CHANNEL, UUID_L2CAP,
    };

    #[test]
    fn build_request_round_trips_through_existing_server_parser() {
        // Use our own server-side parser (sdp.rs) to confirm the bytes
        // we emit are actually parseable. Catches any byte-order
        // mistakes without needing a phone in the loop.
        let bytes = build_service_search_attribute_request(0x1234, UUID_PBAP_PSE, 0xffff);
        let parsed =
            crate::aokie_radio::sdp::parse_service_search_attribute_request(&bytes).unwrap();
        assert_eq!(parsed.transaction_id, 0x1234);
        assert_eq!(parsed.maximum_attribute_byte_count, 0xffff);
        // Search pattern: a single Uuid16 element. Should be 0x19, 0x11, 0x2f.
        assert_eq!(parsed.service_search_pattern, &[0x19, 0x11, 0x2f]);
    }

    #[test]
    fn parse_response_extracts_attribute_lists_and_continuation() {
        // Drive the existing server (HFP record) and parse its reply.
        let server = SdpServer::new_with_hfp();
        let request = build_service_search_attribute_request(0xabcd, 0x111e, 0xffff);
        let response_bytes = server.handle_request(&request).unwrap();
        let resp = parse_service_search_attribute_response(&response_bytes).unwrap();
        assert_eq!(resp.transaction_id, 0xabcd);
        assert!(resp.continuation_state.is_empty());
        assert!(!resp.attribute_lists.is_empty());

        // Walk records — should be one (HFP) and contain the service name.
        let records = iter_records(resp.attribute_lists).unwrap();
        assert_eq!(records.len(), 1);
        let svc_name_attr = find_attribute(records[0], 0x0100).unwrap().unwrap();
        // Service name is a Text DataElement; its bytes start with 0x25 descriptor.
        assert_eq!(svc_name_attr[0], 0x25);
    }

    #[test]
    fn extract_rfcomm_channel_from_hfp_record() {
        // The HFP service record we ourselves serve advertises RFCOMM
        // on AOKIE_RFCOMM_CHANNEL (1). The client-side extractor must
        // recover that value when handed the inner attribute payload.
        let outer_record = aokie_hfp_service_record();
        // Strip the outer Sequence wrapper so we have the bare
        // "(AttrId, AttrVal)*" stream `find_attribute` expects.
        let (parsed, _) = parse_data_element(&outer_record).unwrap();
        let inner = match parsed {
            ParsedDataElement::Sequence(b) => b,
            _ => panic!("HFP record root must be a sequence"),
        };
        let channel = extract_rfcomm_channel(inner).unwrap();
        assert_eq!(channel, Some(AOKIE_RFCOMM_CHANNEL));
    }

    #[test]
    fn extract_rfcomm_channel_returns_none_when_record_lacks_protocol_list() {
        // A record that has no ProtocolDescriptorList (legal — the
        // spec only mandates ServiceClassIdList). The extractor must
        // return Ok(None), not Err.
        let mut attrs = Vec::new();
        let id = DataElement::Uint16(0x0001); // ServiceClassIdList
        encode_data_element(id, &mut attrs);
        let mut value = Vec::new();
        encode_data_element(DataElement::Uuid16(UUID_PBAP_PSE), &mut value);
        encode_data_element(DataElement::Sequence(&value), &mut attrs);
        let channel = extract_rfcomm_channel(&attrs).unwrap();
        assert_eq!(channel, None);
    }

    #[test]
    fn extract_rfcomm_channel_handles_uint16_uuid_encoding() {
        // Some real Android Bluedroid versions encode L2CAP/RFCOMM
        // protocol UUIDs with descriptor 0x09 (Uint16) rather than
        // 0x19 (Uuid16) inside the ProtocolDescriptorList. The
        // extractor must accept both encodings.
        let mut record = Vec::new();
        encode_data_element(
            DataElement::Uint16(ATTR_PROTOCOL_DESCRIPTOR_LIST),
            &mut record,
        );

        let mut l2cap_proto = Vec::new();
        // Use Uint16 for the UUID rather than Uuid16.
        l2cap_proto.push(0x09);
        l2cap_proto.extend_from_slice(&UUID_L2CAP.to_be_bytes());

        let mut rfcomm_proto = Vec::new();
        rfcomm_proto.push(0x09);
        rfcomm_proto.extend_from_slice(&UUID_RFCOMM.to_be_bytes());
        encode_data_element(DataElement::Uint8(19), &mut rfcomm_proto);

        let mut pdl = Vec::new();
        encode_data_element(DataElement::Sequence(&l2cap_proto), &mut pdl);
        encode_data_element(DataElement::Sequence(&rfcomm_proto), &mut pdl);
        encode_data_element(DataElement::Sequence(&pdl), &mut record);

        assert_eq!(extract_rfcomm_channel(&record).unwrap(), Some(19));
    }

    #[test]
    fn parse_response_rejects_truncated_attribute_lists() {
        // A header that claims more bytes than it carries is the most
        // common SDP corruption — phones occasionally send a truncated
        // response when their internal buffer fills up.
        let mut bad = vec![PDU_SERVICE_SEARCH_ATTRIBUTE_RESPONSE, 0x00, 0x01];
        bad.extend_from_slice(&5u16.to_be_bytes()); // param_len = 5
        bad.extend_from_slice(&100u16.to_be_bytes()); // attr_lists_len = 100, way too big
        bad.push(0xab); // 1 byte where 100 are claimed
        bad.push(0xcd);
        bad.push(0x00);
        assert!(parse_service_search_attribute_response(&bad).is_err());
    }

    #[test]
    fn parse_response_rejects_missing_continuation_byte() {
        // Even for a "no continuation" reply, the spec mandates a
        // single 0x00 byte. Truncate-before-continuation must error
        // out, not silently succeed.
        let pdu = vec![
            PDU_SERVICE_SEARCH_ATTRIBUTE_RESPONSE,
            0x00,
            0x02, // tid
            0x00,
            0x02, // param_len 2
            0x00,
            0x00, // attr_lists_len 0
                  // missing continuation byte
        ];
        assert!(parse_service_search_attribute_response(&pdu).is_err());
    }

    #[test]
    fn iter_records_returns_empty_for_empty_outer_sequence() {
        // SDP search that matches nothing returns AttributeLists =
        // an empty sequence. Walking should produce zero records, not
        // error.
        let mut empty_outer = Vec::new();
        encode_data_element(DataElement::Sequence(&[]), &mut empty_outer);
        let records = iter_records(&empty_outer).unwrap();
        assert!(records.is_empty());
    }

    #[test]
    fn end_to_end_pbap_record_round_trip() {
        // Build a synthetic PBAP record with RFCOMM channel 19 the way
        // a real Pixel would advertise, run it through the response
        // builder, parse the response with the client, and recover
        // the channel. This is the round-trip we'd see on real
        // hardware compressed into a unit test.
        const PBAP_CHANNEL: u8 = 19;

        // Inner record: ProtocolDescriptorList carrying L2CAP + RFCOMM(19).
        let mut attrs = Vec::new();
        encode_data_element(
            DataElement::Uint16(ATTR_PROTOCOL_DESCRIPTOR_LIST),
            &mut attrs,
        );
        let mut l2cap_proto = Vec::new();
        encode_data_element(DataElement::Uuid16(UUID_L2CAP), &mut l2cap_proto);
        let mut rfcomm_proto = Vec::new();
        encode_data_element(DataElement::Uuid16(UUID_RFCOMM), &mut rfcomm_proto);
        encode_data_element(DataElement::Uint8(PBAP_CHANNEL), &mut rfcomm_proto);
        let mut pdl = Vec::new();
        encode_data_element(DataElement::Sequence(&l2cap_proto), &mut pdl);
        encode_data_element(DataElement::Sequence(&rfcomm_proto), &mut pdl);
        encode_data_element(DataElement::Sequence(&pdl), &mut attrs);

        // Outer wrap: AttributeLists is a Sequence of inner Sequences.
        let mut inner_sequences = Vec::new();
        encode_data_element(DataElement::Sequence(&attrs), &mut inner_sequences);
        let mut attribute_lists = Vec::new();
        encode_data_element(
            DataElement::Sequence(&inner_sequences),
            &mut attribute_lists,
        );

        // Wire response.
        let response_bytes = service_search_attribute_response(0x1234, &attribute_lists);

        // Client-side parse.
        let resp = parse_service_search_attribute_response(&response_bytes).unwrap();
        assert_eq!(resp.transaction_id, 0x1234);
        let records = iter_records(resp.attribute_lists).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            extract_rfcomm_channel(records[0]).unwrap(),
            Some(PBAP_CHANNEL)
        );
    }

    #[test]
    fn fuzz_parse_response_does_not_panic_on_random_bytes() {
        // SDP responses come from the phone over L2CAP CID 0x0001.
        // The response carries a length-prefixed attribute list and
        // a continuation state — both lengths can lie. Random input
        // must surface as Err, not a panic.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0x5344_5043_4c49_454e);
        for _ in 0..5_000 {
            let len = (rng.next_u32() % 1024) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = parse_service_search_attribute_response(&buf);
        }
    }
}
