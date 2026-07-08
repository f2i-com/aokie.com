pub const PDU_SERVICE_SEARCH_ATTRIBUTE_REQUEST: u8 = 0x06;
pub const PDU_SERVICE_SEARCH_ATTRIBUTE_RESPONSE: u8 = 0x07;

pub const ATTR_SERVICE_RECORD_HANDLE: u16 = 0x0000;
pub const ATTR_SERVICE_CLASS_ID_LIST: u16 = 0x0001;
pub const ATTR_PROTOCOL_DESCRIPTOR_LIST: u16 = 0x0004;
pub const ATTR_BROWSE_GROUP_LIST: u16 = 0x0005;
pub const ATTR_BLUETOOTH_PROFILE_DESCRIPTOR_LIST: u16 = 0x0009;
pub const ATTR_SERVICE_NAME: u16 = 0x0100;
pub const ATTR_SUPPORTED_FEATURES: u16 = 0x0311;

pub const UUID_PUBLIC_BROWSE_ROOT: u16 = 0x1002;
pub const UUID_L2CAP: u16 = 0x0100;
pub const UUID_RFCOMM: u16 = 0x0003;
pub const UUID_GENERIC_AUDIO: u16 = 0x1203;
pub const UUID_HANDSFREE: u16 = 0x111e;
/// MAP — Message Access Profile (used as the *profile* identifier on
/// the BluetoothProfileDescriptorList of any MAP record we publish).
pub const UUID_MAP_PROFILE: u16 = 0x1134;
/// MAP — Message Notification Server. We advertise this when we want
/// the phone to push EventReports to us on a new SMS.
pub const UUID_MAP_MNS_SERVER: u16 = 0x1133;
/// PnP Device Identification profile. Phones (Pixel especially) probe
/// for this record on first contact to learn the device vendor/product.
/// Without it, some stacks abort discovery before they even ask about
/// HFP-HF, and tear the ACL down with reason 0x13 ("remote user
/// terminated"). Cheap to advertise, expensive to omit.
pub const UUID_PNP_INFORMATION: u16 = 0x1200;

pub const AOKIE_HFP_SERVICE_RECORD_HANDLE: u32 = 0x0001_0001;
pub const AOKIE_RFCOMM_CHANNEL: u8 = 1;
pub const AOKIE_HFP_PROFILE_VERSION: u16 = 0x0109;
pub const AOKIE_HFP_SUPPORTED_FEATURES: u16 = 0x003f;
pub const AOKIE_HFP_SERVICE_NAME: &str = "Aokie AI Assistant";

/// MNS server record — assigned a different SDP record handle and a
/// distinct RFCOMM server channel from HFP so the phone can multiplex
/// HFP and MNS over the same ACL without channel reuse.
pub const AOKIE_MNS_SERVICE_RECORD_HANDLE: u32 = 0x0001_0002;
pub const AOKIE_MNS_RFCOMM_CHANNEL: u8 = 2;
pub const AOKIE_MNS_PROFILE_VERSION: u16 = 0x0104; // MAP 1.4
/// MAP-MCE SupportedFeatures bitmask. Per MAP 1.4 §7.1.1:
///   bit 0 = Notification Registration (REQUIRED for any MCE)
///   bit 1 = Notification (REQUIRED for any MCE)
///   bit 2 = Browsing
/// Earlier value of 0x06 was off-by-one (missing bit 0); some Android
/// builds reject an MCE record whose features don't claim Notification
/// Registration and quietly drop the device from the MAP profile list,
/// which suppresses the per-device "Text messages" permission toggle.
pub const AOKIE_MNS_SUPPORTED_FEATURES: u32 = 0x0000_0007;
pub const AOKIE_MNS_SERVICE_NAME: &str = "Aokie MNS";

/// PnP Device Identification record — advertised so phones that probe
/// for 0x1200 on first contact get a non-empty reply and continue with
/// the rest of their service discovery sweep.
pub const AOKIE_PNP_SERVICE_RECORD_HANDLE: u32 = 0x0001_0003;
/// PnP Information specification version 1.3 (BCD: 0x0103).
pub const AOKIE_PNP_SPEC_ID: u16 = 0x0103;
/// VendorIDSource = 2 (USB Implementer's Forum). USB-IF VIDs are the
/// path BlueZ defaults take, and phones don't care which source we
/// declare — they just need *some* identifier.
pub const AOKIE_PNP_VENDOR_ID_SOURCE: u16 = 0x0002;
/// USB-IF VID 0x1D6B = "Linux Foundation". Same value BlueZ ships with
/// out of the box, so it reads as a plausible PC-side source to phones.
pub const AOKIE_PNP_VENDOR_ID: u16 = 0x1D6B;
/// Aokie product ID — picked to be unique within our VID namespace.
/// Bumps independently of the app version.
pub const AOKIE_PNP_PRODUCT_ID: u16 = 0xA0E1;
/// BCD product version — track against `package.json` major.minor.
pub const AOKIE_PNP_PRODUCT_VERSION: u16 = 0x0202;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSearchAttributeRequest<'a> {
    pub transaction_id: u16,
    pub service_search_pattern: &'a [u8],
    pub maximum_attribute_byte_count: u16,
    pub attribute_id_list: &'a [u8],
    pub continuation_state: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataElement<'a> {
    Uint8(u8),
    Uint16(u16),
    Uint32(u32),
    Uuid16(u16),
    /// Boolean (8-bit) — descriptor 0x28. Used by PnP Device ID's
    /// PrimaryRecord attribute (0x0204).
    Bool(bool),
    Text(&'a str),
    Sequence(&'a [u8]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedDataElement<'a> {
    Uint8(u8),
    Uint16(u16),
    Uint32(u32),
    Uuid16(u16),
    Bool(bool),
    Text(&'a [u8]),
    Sequence(&'a [u8]),
}

pub fn encode_data_element(element: DataElement<'_>, out: &mut Vec<u8>) {
    match element {
        DataElement::Uint8(value) => {
            out.push(0x08);
            out.push(value);
        }
        DataElement::Uint16(value) => {
            out.push(0x09);
            out.extend_from_slice(&value.to_be_bytes());
        }
        DataElement::Uint32(value) => {
            out.push(0x0a);
            out.extend_from_slice(&value.to_be_bytes());
        }
        DataElement::Uuid16(value) => {
            out.push(0x19);
            out.extend_from_slice(&value.to_be_bytes());
        }
        DataElement::Bool(value) => {
            out.push(0x28);
            out.push(if value { 1 } else { 0 });
        }
        // SDP variable-length elements come in three width tiers — 8-bit
        // (0x25/0x35), 16-bit (0x26/0x36), 32-bit (0x27/0x37). Pre-fix
        // the encoder always used the 8-bit form and silently truncated
        // `len() as u8` past 255 bytes, producing packets the peer would
        // either reject or mis-decode. Pick the narrowest descriptor
        // that fits.
        DataElement::Text(value) => {
            encode_variable_element(0x25, value.as_bytes(), out);
        }
        DataElement::Sequence(value) => {
            encode_variable_element(0x35, value, out);
        }
    }
}

fn encode_variable_element(base_descriptor: u8, payload: &[u8], out: &mut Vec<u8>) {
    let len = payload.len();
    if len <= u8::MAX as usize {
        out.push(base_descriptor);
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(base_descriptor + 1);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(base_descriptor + 2);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
    out.extend_from_slice(payload);
}

pub fn parse_data_element(input: &[u8]) -> Result<(ParsedDataElement<'_>, usize), String> {
    let descriptor = *input
        .first()
        .ok_or_else(|| "SDP data element is empty".to_string())?;
    match descriptor {
        0x08 => {
            require_len(input, 2, "SDP uint8")?;
            Ok((ParsedDataElement::Uint8(input[1]), 2))
        }
        0x09 => {
            require_len(input, 3, "SDP uint16")?;
            Ok((
                ParsedDataElement::Uint16(u16::from_be_bytes([input[1], input[2]])),
                3,
            ))
        }
        0x0a => {
            require_len(input, 5, "SDP uint32")?;
            Ok((
                ParsedDataElement::Uint32(u32::from_be_bytes([
                    input[1], input[2], input[3], input[4],
                ])),
                5,
            ))
        }
        0x19 => {
            require_len(input, 3, "SDP uuid16")?;
            Ok((
                ParsedDataElement::Uuid16(u16::from_be_bytes([input[1], input[2]])),
                3,
            ))
        }
        0x28 => {
            require_len(input, 2, "SDP boolean")?;
            Ok((ParsedDataElement::Bool(input[1] != 0), 2))
        }
        // 8/16/32-bit length tiers for both Text (0x25/0x26/0x27) and
        // Sequence (0x35/0x36/0x37). The encoder picks the narrowest
        // tier that fits, so the parser has to recognize all three to
        // round-trip its own output and to handle real peer responses
        // that include long service names or option lists.
        0x25 | 0x26 | 0x27 | 0x35 | 0x36 | 0x37 => {
            let header_len = match descriptor & 0x07 {
                0x05 => 2,
                0x06 => 3,
                0x07 => 5,
                _ => unreachable!(),
            };
            require_len(input, header_len, "SDP variable element")?;
            let len = match header_len {
                2 => input[1] as usize,
                3 => u16::from_be_bytes([input[1], input[2]]) as usize,
                5 => u32::from_be_bytes([input[1], input[2], input[3], input[4]]) as usize,
                _ => unreachable!(),
            };
            require_len(input, header_len + len, "SDP variable element payload")?;
            let payload = &input[header_len..header_len + len];
            let total = header_len + len;
            if descriptor < 0x30 {
                Ok((ParsedDataElement::Text(payload), total))
            } else {
                Ok((ParsedDataElement::Sequence(payload), total))
            }
        }
        _ => Err(format!(
            "unsupported SDP data element descriptor 0x{:02x}",
            descriptor
        )),
    }
}

/// A registered SDP service record.
///
/// `searchable_uuids` is the flat set of 16-bit UUIDs that appear
/// *anywhere* in the record — service class UUIDs, protocol UUIDs (L2CAP,
/// RFCOMM, OBEX), profile UUIDs, browse-group UUIDs. Per SDP §2.5.1, a
/// service search pattern matches a record if every UUID in the pattern
/// appears anywhere in the record — not just in ServiceClassIDList. This
/// matters in practice: phones routinely probe with `[L2CAP]` (0x0100)
/// or `[PublicBrowseRoot]` (0x1002) as a generic "what do you have"
/// sweep, and an empty reply makes some stacks (Pixel 10a / Bluedroid
/// in particular) abort discovery and tear the ACL down.
///
/// `encoded_record` is the pre-encoded inner `DataElementSequence` of
/// `(AttrId, AttrVal)` pairs, ascending by AttrId — i.e. the byte slice
/// you'd embed inside the response's outer AttributeLists sequence.
#[derive(Debug, Clone)]
pub struct ServiceRecord {
    pub handle: u32,
    pub searchable_uuids: Vec<u16>,
    pub encoded_record: Vec<u8>,
}

impl ServiceRecord {
    /// Build a ServiceRecord from the encoded attribute pairs, computing
    /// `searchable_uuids` by walking the structure for every Uuid16. Use
    /// this in preference to constructing the struct field-by-field — it
    /// guarantees the search index stays in sync with the encoded bytes.
    pub fn new(handle: u32, encoded_record: Vec<u8>) -> Self {
        let mut uuids = Vec::new();
        // The encoded record is a flat (AttrId, AttrVal)+ stream — not
        // wrapped in an outer Sequence — so feed it straight in.
        let _ = collect_uuid16s(&encoded_record, &mut uuids);
        uuids.sort_unstable();
        uuids.dedup();
        Self {
            handle,
            searchable_uuids: uuids,
            encoded_record,
        }
    }
}

/// Walk a stream of SDP data elements and append every Uuid16 found,
/// recursing into Sequences. Text/Uint/Bool/Uuid32/Uuid128 payloads are
/// skipped — we only collect 16-bit UUIDs because (a) phones we target
/// only ever search by Uuid16 and (b) walking via parse_data_element
/// avoids matching 0x19 bytes that happen to sit inside a Text payload.
fn collect_uuid16s(input: &[u8], out: &mut Vec<u16>) -> Result<(), String> {
    let mut offset = 0;
    while offset < input.len() {
        let (element, used) = parse_data_element(&input[offset..])?;
        match element {
            ParsedDataElement::Uuid16(u) => out.push(u),
            ParsedDataElement::Sequence(inner) => collect_uuid16s(inner, out)?,
            _ => {}
        }
        offset += used;
    }
    Ok(())
}

/// In-memory SDP record registry. Phase 0a of MAP/PBAP support — HFP is
/// the only tenant today, but `register()` is the seam new profiles plug
/// into without touching the request handler.
#[derive(Debug, Default)]
pub struct SdpServer {
    records: Vec<ServiceRecord>,
}

impl SdpServer {
    /// New SdpServer pre-populated with the HFP record. Other profiles
    /// add their records via `register`.
    pub fn new_with_hfp() -> Self {
        let mut s = Self::default();
        s.register(hfp_service_record());
        s
    }

    pub fn register(&mut self, record: ServiceRecord) {
        self.records.push(record);
    }

    pub fn records(&self) -> &[ServiceRecord] {
        &self.records
    }

    /// Find every record whose `searchable_uuids` is a superset of the
    /// search-pattern UUIDs (SDP §2.5.1).
    pub fn match_records(&self, pattern_uuids: &[u16]) -> Vec<&ServiceRecord> {
        self.records
            .iter()
            .filter(|r| pattern_uuids.iter().all(|u| r.searchable_uuids.contains(u)))
            .collect()
    }

    /// Build a complete ServiceSearchAttributeResponse for a parsed
    /// request, filtering registered records by the request's pattern.
    pub fn handle_request(&self, packet: &[u8]) -> Result<Vec<u8>, String> {
        let request = parse_service_search_attribute_request(packet)?;
        let pattern_uuids = parse_uuid_search_pattern(request.service_search_pattern)?;
        let matched = self.match_records(&pattern_uuids);

        // SDP §4.7: AttributeLists is a *Sequence-of-Sequences* — one
        // inner sequence of (AttrId, AttrVal) pairs per matched record,
        // all wrapped in an outer sequence. ServiceRecord.encoded_record
        // is the raw attribute-pair bytes (no wrapper), so we wrap each
        // once for the inner, then wrap the concatenation once more for
        // the outer.
        let mut inner_sequences = Vec::new();
        for r in &matched {
            encode_data_element(
                DataElement::Sequence(&r.encoded_record),
                &mut inner_sequences,
            );
        }
        let mut attribute_lists = Vec::with_capacity(inner_sequences.len() + 4);
        encode_data_element(
            DataElement::Sequence(&inner_sequences),
            &mut attribute_lists,
        );

        let max = request.maximum_attribute_byte_count as usize;
        if max < attribute_lists.len() {
            // Continuation state isn't implemented; in practice the HFP
            // record is ~150 B and every real peer asks for max >= 600.
            // Log so we know if a peer ever pinches us.
            eprintln!(
                "[AokieRadio] SDP peer requested max {}B but our matched records are {}B — sending full reply (continuation state not implemented)",
                max,
                attribute_lists.len()
            );
        }
        eprintln!(
            "[AokieRadio] SDP query: pattern={:04x?}, matched {}/{} records, replying {}B",
            pattern_uuids,
            matched.len(),
            self.records.len(),
            attribute_lists.len(),
        );

        Ok(service_search_attribute_response(
            request.transaction_id,
            &attribute_lists,
        ))
    }
}

/// Parse the inner of a ServiceSearchPattern (already unwrapped from its
/// outer Sequence by `parse_service_search_attribute_request`) into a
/// flat list of 16-bit UUIDs. Spec allows 32- and 128-bit UUIDs in the
/// pattern too, but real-world phone queries always use Uuid16; we error
/// loudly if we ever see something else so we know to extend support.
fn parse_uuid_search_pattern(pattern: &[u8]) -> Result<Vec<u16>, String> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < pattern.len() {
        let (element, used) = parse_data_element(&pattern[offset..])?;
        match element {
            ParsedDataElement::Uuid16(u) => out.push(u),
            other => {
                return Err(format!(
                    "SDP search pattern contained non-Uuid16 element {:?} (32/128-bit UUIDs not supported yet)",
                    other
                ));
            }
        }
        offset += used;
    }
    Ok(out)
}

/// Build the HFP ServiceRecord for registration.
pub fn hfp_service_record() -> ServiceRecord {
    ServiceRecord::new(
        AOKIE_HFP_SERVICE_RECORD_HANDLE,
        build_hfp_attribute_sequence(),
    )
}

/// Build the MNS server ServiceRecord. We advertise this so the phone
/// (acting as MNS client) knows which RFCOMM server channel to push
/// EventReport packets to once it has subscribed via MAS
/// SetNotificationRegistration. Mirrors HFP's record shape — only the
/// service-class UUID, the RFCOMM channel number, and the profile
/// descriptor differ.
pub fn mns_service_record() -> ServiceRecord {
    ServiceRecord::new(
        AOKIE_MNS_SERVICE_RECORD_HANDLE,
        build_mns_attribute_sequence(),
    )
}

/// Build the PnP Device Identification ServiceRecord. The phone uses
/// this to learn vendor/product/version of whatever it just paired
/// with; some stacks treat its absence as "this device is broken" and
/// drop the ACL before exploring HFP. See AOKIE_PNP_* constants for the
/// values we publish.
pub fn pnp_device_id_service_record() -> ServiceRecord {
    ServiceRecord::new(
        AOKIE_PNP_SERVICE_RECORD_HANDLE,
        build_pnp_attribute_sequence(),
    )
}

fn build_mns_attribute_sequence() -> Vec<u8> {
    let mut attributes = Vec::new();
    add_attribute(&mut attributes, ATTR_SERVICE_RECORD_HANDLE, |out| {
        encode_data_element(DataElement::Uint32(AOKIE_MNS_SERVICE_RECORD_HANDLE), out);
    });
    add_attribute(&mut attributes, ATTR_SERVICE_CLASS_ID_LIST, |out| {
        let mut seq = Vec::new();
        encode_data_element(DataElement::Uuid16(UUID_MAP_MNS_SERVER), &mut seq);
        encode_data_element(DataElement::Sequence(&seq), out);
    });
    add_attribute(&mut attributes, ATTR_PROTOCOL_DESCRIPTOR_LIST, |out| {
        let mut l2cap = Vec::new();
        encode_data_element(DataElement::Uuid16(UUID_L2CAP), &mut l2cap);

        let mut rfcomm = Vec::new();
        encode_data_element(DataElement::Uuid16(UUID_RFCOMM), &mut rfcomm);
        encode_data_element(DataElement::Uint8(AOKIE_MNS_RFCOMM_CHANNEL), &mut rfcomm);

        // OBEX is the protocol layered on top of RFCOMM for MAP.
        // Adding it to the descriptor list is what tells the phone
        // "speak OBEX once you're past the RFCOMM mux".
        let mut obex = Vec::new();
        encode_data_element(DataElement::Uuid16(0x0008), &mut obex); // UUID_OBEX

        let mut seq = Vec::new();
        encode_data_element(DataElement::Sequence(&l2cap), &mut seq);
        encode_data_element(DataElement::Sequence(&rfcomm), &mut seq);
        encode_data_element(DataElement::Sequence(&obex), &mut seq);
        encode_data_element(DataElement::Sequence(&seq), out);
    });
    add_attribute(&mut attributes, ATTR_BROWSE_GROUP_LIST, |out| {
        let mut seq = Vec::new();
        encode_data_element(DataElement::Uuid16(UUID_PUBLIC_BROWSE_ROOT), &mut seq);
        encode_data_element(DataElement::Sequence(&seq), out);
    });
    add_attribute(
        &mut attributes,
        ATTR_BLUETOOTH_PROFILE_DESCRIPTOR_LIST,
        |out| {
            // Profile descriptor uses the *profile* UUID (0x1134),
            // NOT the service-class UUID — that's a quirk of MAP that
            // catches everyone the first time.
            let mut profile = Vec::new();
            encode_data_element(DataElement::Uuid16(UUID_MAP_PROFILE), &mut profile);
            encode_data_element(DataElement::Uint16(AOKIE_MNS_PROFILE_VERSION), &mut profile);
            let mut seq = Vec::new();
            encode_data_element(DataElement::Sequence(&profile), &mut seq);
            encode_data_element(DataElement::Sequence(&seq), out);
        },
    );
    add_attribute(&mut attributes, ATTR_SERVICE_NAME, |out| {
        encode_data_element(DataElement::Text(AOKIE_MNS_SERVICE_NAME), out);
    });
    add_attribute(&mut attributes, ATTR_SUPPORTED_FEATURES, |out| {
        encode_data_element(DataElement::Uint32(AOKIE_MNS_SUPPORTED_FEATURES), out);
    });
    attributes
}

fn build_pnp_attribute_sequence() -> Vec<u8> {
    let mut attributes = Vec::new();
    add_attribute(&mut attributes, ATTR_SERVICE_RECORD_HANDLE, |out| {
        encode_data_element(DataElement::Uint32(AOKIE_PNP_SERVICE_RECORD_HANDLE), out);
    });
    add_attribute(&mut attributes, ATTR_SERVICE_CLASS_ID_LIST, |out| {
        let mut seq = Vec::new();
        encode_data_element(DataElement::Uuid16(UUID_PNP_INFORMATION), &mut seq);
        encode_data_element(DataElement::Sequence(&seq), out);
    });
    add_attribute(&mut attributes, ATTR_BROWSE_GROUP_LIST, |out| {
        let mut seq = Vec::new();
        encode_data_element(DataElement::Uuid16(UUID_PUBLIC_BROWSE_ROOT), &mut seq);
        encode_data_element(DataElement::Sequence(&seq), out);
    });
    add_attribute(
        &mut attributes,
        ATTR_BLUETOOTH_PROFILE_DESCRIPTOR_LIST,
        |out| {
            let mut profile = Vec::new();
            encode_data_element(DataElement::Uuid16(UUID_PNP_INFORMATION), &mut profile);
            encode_data_element(DataElement::Uint16(AOKIE_PNP_SPEC_ID), &mut profile);
            let mut seq = Vec::new();
            encode_data_element(DataElement::Sequence(&profile), &mut seq);
            encode_data_element(DataElement::Sequence(&seq), out);
        },
    );
    // PnP attributes 0x0200-0x0205 — order matters: SDP attributes must
    // appear in ascending AttrId order.
    add_attribute(&mut attributes, 0x0200, |out| {
        encode_data_element(DataElement::Uint16(AOKIE_PNP_SPEC_ID), out);
    });
    add_attribute(&mut attributes, 0x0201, |out| {
        encode_data_element(DataElement::Uint16(AOKIE_PNP_VENDOR_ID), out);
    });
    add_attribute(&mut attributes, 0x0202, |out| {
        encode_data_element(DataElement::Uint16(AOKIE_PNP_PRODUCT_ID), out);
    });
    add_attribute(&mut attributes, 0x0203, |out| {
        encode_data_element(DataElement::Uint16(AOKIE_PNP_PRODUCT_VERSION), out);
    });
    // PrimaryRecord = true. There's only one PnP record for this device,
    // so it's the primary by definition.
    add_attribute(&mut attributes, 0x0204, |out| {
        encode_data_element(DataElement::Bool(true), out);
    });
    add_attribute(&mut attributes, 0x0205, |out| {
        encode_data_element(DataElement::Uint16(AOKIE_PNP_VENDOR_ID_SOURCE), out);
    });
    attributes
}

/// Backwards-compat: returns the HFP attribute pairs already wrapped in
/// one DataElementSequence — i.e. the inner sequence that goes inside
/// the response's outer AttributeLists. New code should construct a
/// `ServiceRecord` and register it on an `SdpServer` instead.
pub fn aokie_hfp_service_record() -> Vec<u8> {
    let attrs = build_hfp_attribute_sequence();
    let mut wrapped = Vec::with_capacity(attrs.len() + 4);
    encode_data_element(DataElement::Sequence(&attrs), &mut wrapped);
    wrapped
}

fn build_hfp_attribute_sequence() -> Vec<u8> {
    let mut attributes = Vec::new();
    add_attribute(&mut attributes, ATTR_SERVICE_RECORD_HANDLE, |out| {
        encode_data_element(DataElement::Uint32(AOKIE_HFP_SERVICE_RECORD_HANDLE), out);
    });
    add_attribute(&mut attributes, ATTR_SERVICE_CLASS_ID_LIST, |out| {
        let mut seq = Vec::new();
        encode_data_element(DataElement::Uuid16(UUID_HANDSFREE), &mut seq);
        encode_data_element(DataElement::Uuid16(UUID_GENERIC_AUDIO), &mut seq);
        encode_data_element(DataElement::Sequence(&seq), out);
    });
    add_attribute(&mut attributes, ATTR_PROTOCOL_DESCRIPTOR_LIST, |out| {
        let mut l2cap = Vec::new();
        encode_data_element(DataElement::Uuid16(UUID_L2CAP), &mut l2cap);

        let mut rfcomm = Vec::new();
        encode_data_element(DataElement::Uuid16(UUID_RFCOMM), &mut rfcomm);
        encode_data_element(DataElement::Uint8(AOKIE_RFCOMM_CHANNEL), &mut rfcomm);

        let mut seq = Vec::new();
        encode_data_element(DataElement::Sequence(&l2cap), &mut seq);
        encode_data_element(DataElement::Sequence(&rfcomm), &mut seq);
        encode_data_element(DataElement::Sequence(&seq), out);
    });
    add_attribute(&mut attributes, ATTR_BROWSE_GROUP_LIST, |out| {
        let mut seq = Vec::new();
        encode_data_element(DataElement::Uuid16(UUID_PUBLIC_BROWSE_ROOT), &mut seq);
        encode_data_element(DataElement::Sequence(&seq), out);
    });
    add_attribute(
        &mut attributes,
        ATTR_BLUETOOTH_PROFILE_DESCRIPTOR_LIST,
        |out| {
            let mut profile = Vec::new();
            encode_data_element(DataElement::Uuid16(UUID_HANDSFREE), &mut profile);
            encode_data_element(DataElement::Uint16(AOKIE_HFP_PROFILE_VERSION), &mut profile);
            let mut seq = Vec::new();
            encode_data_element(DataElement::Sequence(&profile), &mut seq);
            encode_data_element(DataElement::Sequence(&seq), out);
        },
    );
    add_attribute(&mut attributes, ATTR_SERVICE_NAME, |out| {
        encode_data_element(DataElement::Text(AOKIE_HFP_SERVICE_NAME), out);
    });
    add_attribute(&mut attributes, ATTR_SUPPORTED_FEATURES, |out| {
        encode_data_element(DataElement::Uint16(AOKIE_HFP_SUPPORTED_FEATURES), out);
    });
    attributes
}

pub fn parse_service_search_attribute_request(
    packet: &[u8],
) -> Result<ServiceSearchAttributeRequest<'_>, String> {
    require_len(packet, 5, "SDP PDU header")?;
    if packet[0] != PDU_SERVICE_SEARCH_ATTRIBUTE_REQUEST {
        return Err(format!(
            "expected ServiceSearchAttributeRequest PDU 0x06, got 0x{:02x}",
            packet[0]
        ));
    }

    let transaction_id = u16::from_be_bytes([packet[1], packet[2]]);
    let param_len = u16::from_be_bytes([packet[3], packet[4]]) as usize;
    require_len(packet, 5 + param_len, "SDP PDU parameters")?;
    let params = &packet[5..5 + param_len];

    let (service_search_pattern, used) = parse_data_element(params)?;
    let ParsedDataElement::Sequence(service_search_pattern) = service_search_pattern else {
        return Err("SDP service search pattern is not a sequence".to_string());
    };
    let mut offset = used;

    require_len(&params[offset..], 2, "SDP MaximumAttributeByteCount")?;
    let maximum_attribute_byte_count = u16::from_be_bytes([params[offset], params[offset + 1]]);
    offset += 2;

    let (attribute_id_list, used) = parse_data_element(&params[offset..])?;
    let ParsedDataElement::Sequence(attribute_id_list) = attribute_id_list else {
        return Err("SDP attribute ID list is not a sequence".to_string());
    };
    offset += used;

    require_len(&params[offset..], 1, "SDP continuation state length")?;
    let continuation_len = params[offset] as usize;
    offset += 1;
    require_len(
        &params[offset..],
        continuation_len,
        "SDP continuation state",
    )?;
    let continuation_state = &params[offset..offset + continuation_len];

    Ok(ServiceSearchAttributeRequest {
        transaction_id,
        service_search_pattern,
        maximum_attribute_byte_count,
        attribute_id_list,
        continuation_state,
    })
}

/// Backwards-compat wrapper used by `l2cap.rs` until Phase 0b moves
/// SdpServer ownership up into the L2CAP layer. Builds a fresh server
/// on every call — the registry is small and rebuild cost is
/// negligible compared to actual ACL latency. Includes HFP, the MAP MNS
/// server record, and PnP Device Identification so phones (Pixel/
/// Bluedroid in particular) get a non-empty reply to their initial
/// 0x1200 sweep and continue with the rest of discovery.
pub fn handle_service_search_attribute_request(packet: &[u8]) -> Result<Vec<u8>, String> {
    let mut server = SdpServer::new_with_hfp();
    server.register(mns_service_record());
    server.register(pnp_device_id_service_record());
    server.handle_request(packet)
}

pub fn service_search_attribute_response(transaction_id: u16, attribute_list: &[u8]) -> Vec<u8> {
    let mut params = Vec::with_capacity(3 + attribute_list.len());
    params.extend_from_slice(&(attribute_list.len() as u16).to_be_bytes());
    params.extend_from_slice(attribute_list);
    params.push(0); // no continuation state

    let mut pdu = Vec::with_capacity(5 + params.len());
    pdu.push(PDU_SERVICE_SEARCH_ATTRIBUTE_RESPONSE);
    pdu.extend_from_slice(&transaction_id.to_be_bytes());
    pdu.extend_from_slice(&(params.len() as u16).to_be_bytes());
    pdu.extend_from_slice(&params);
    pdu
}

fn add_attribute(out: &mut Vec<u8>, attribute_id: u16, encode_value: impl FnOnce(&mut Vec<u8>)) {
    encode_data_element(DataElement::Uint16(attribute_id), out);
    encode_value(out);
}

fn require_len(input: &[u8], min_len: usize, name: &str) -> Result<(), String> {
    if input.len() < min_len {
        return Err(format!(
            "{} is too short: expected {} bytes, got {}",
            name,
            min_len,
            input.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_parses_basic_data_elements() {
        let mut out = Vec::new();
        encode_data_element(DataElement::Uint16(0x111e), &mut out);
        assert_eq!(out, [0x09, 0x11, 0x1e]);
        assert_eq!(
            parse_data_element(&out).unwrap(),
            (ParsedDataElement::Uint16(0x111e), 3)
        );

        out.clear();
        encode_data_element(DataElement::Uuid16(UUID_RFCOMM), &mut out);
        assert_eq!(out, [0x19, 0x00, 0x03]);
        assert_eq!(
            parse_data_element(&out).unwrap(),
            (ParsedDataElement::Uuid16(UUID_RFCOMM), 3)
        );

        out.clear();
        encode_data_element(DataElement::Text("Aokie"), &mut out);
        assert_eq!(out, [0x25, 0x05, b'A', b'o', b'k', b'i', b'e']);
        assert_eq!(
            parse_data_element(&out).unwrap(),
            (ParsedDataElement::Text(b"Aokie"), 7)
        );
    }

    #[test]
    fn promotes_text_and_sequence_descriptors_when_payload_exceeds_8_bit_length() {
        // 256 bytes — the smallest size that overflowed the pre-fix
        // `len() as u8` truncation.
        let long_text = "A".repeat(256);
        let mut out = Vec::new();
        encode_data_element(DataElement::Text(&long_text), &mut out);
        assert_eq!(out[0], 0x26, "expected 16-bit Text descriptor");
        assert_eq!(&out[1..3], &(256u16).to_be_bytes());
        let (parsed, used) = parse_data_element(&out).unwrap();
        assert_eq!(used, out.len());
        assert_eq!(parsed, ParsedDataElement::Text(long_text.as_bytes()));

        // 70_000 bytes — promotes to 32-bit. We don't actually ship
        // anything this large, but the encoder must not silently lose
        // bytes if a future caller does.
        let huge_payload = vec![0u8; 70_000];
        let mut out = Vec::new();
        encode_data_element(DataElement::Sequence(&huge_payload), &mut out);
        assert_eq!(out[0], 0x37, "expected 32-bit Sequence descriptor");
        assert_eq!(&out[1..5], &(70_000u32).to_be_bytes());
        let (parsed, used) = parse_data_element(&out).unwrap();
        assert_eq!(used, out.len());
        assert_eq!(parsed, ParsedDataElement::Sequence(&huge_payload));
    }

    #[test]
    fn builds_hfp_service_record_with_expected_attributes() {
        let record = aokie_hfp_service_record();
        let (root, used) = parse_data_element(&record).unwrap();
        assert_eq!(used, record.len());
        let ParsedDataElement::Sequence(attrs) = root else {
            panic!("record root is not a sequence");
        };

        assert!(attrs.windows(3).any(|w| w == [0x09, 0x00, 0x00]));
        assert!(attrs.windows(3).any(|w| w == [0x09, 0x00, 0x04]));
        assert!(attrs.windows(3).any(|w| w == [0x09, 0x03, 0x11]));
        assert!(attrs.windows(3).any(|w| w == [0x19, 0x11, 0x1e]));
        assert!(attrs.windows(3).any(|w| w == [0x19, 0x12, 0x03]));
        assert!(attrs.windows(3).any(|w| w == [0x19, 0x00, 0x03]));
        assert!(attrs
            .windows(AOKIE_HFP_SERVICE_NAME.len())
            .any(|w| w == AOKIE_HFP_SERVICE_NAME.as_bytes()));
        assert!(attrs.windows(3).any(|w| w == [0x09, 0x00, 0x3f]));
    }

    #[test]
    fn builds_mns_service_record_with_expected_uuid_and_channel() {
        let record = mns_service_record();
        assert_eq!(record.handle, AOKIE_MNS_SERVICE_RECORD_HANDLE);
        // searchable_uuids is the *flat* set of every Uuid16 in the
        // record — service class, protocol stack, browse group, profile
        // — so the spec-compliant matcher (§2.5.1) finds this record
        // when the phone searches by L2CAP, OBEX, or PublicBrowseRoot.
        assert!(record.searchable_uuids.contains(&UUID_MAP_MNS_SERVER));
        assert!(record.searchable_uuids.contains(&UUID_L2CAP));
        assert!(record.searchable_uuids.contains(&UUID_RFCOMM));
        assert!(record.searchable_uuids.contains(&0x0008)); // OBEX
        assert!(record.searchable_uuids.contains(&UUID_PUBLIC_BROWSE_ROOT));
        assert!(record.searchable_uuids.contains(&UUID_MAP_PROFILE));

        // Wrap to inspect the inner attribute sequence.
        let mut wrapped = Vec::new();
        encode_data_element(DataElement::Sequence(&record.encoded_record), &mut wrapped);
        let (root, _used) = parse_data_element(&wrapped).unwrap();
        let ParsedDataElement::Sequence(attrs) = root else {
            panic!("MNS record root is not a sequence");
        };

        // ServiceClassIdList must contain MNS server UUID 0x1133.
        assert!(
            attrs.windows(3).any(|w| w == [0x19, 0x11, 0x33]),
            "expected MNS server UUID 0x1133 in ServiceClassIdList"
        );
        // ProtocolDescriptorList must include the RFCOMM channel byte
        // 0x02 — phones look this up to know where to push notifications.
        assert!(
            attrs
                .windows(2)
                .any(|w| w == [0x08, AOKIE_MNS_RFCOMM_CHANNEL]),
            "expected RFCOMM channel {} in ProtocolDescriptorList",
            AOKIE_MNS_RFCOMM_CHANNEL
        );
        // OBEX UUID 0x0008 must be in the protocol descriptor list.
        assert!(
            attrs.windows(3).any(|w| w == [0x19, 0x00, 0x08]),
            "expected OBEX UUID 0x0008 in ProtocolDescriptorList"
        );
        // BluetoothProfileDescriptorList must point at the MAP profile
        // UUID 0x1134, NOT the MNS service-class UUID.
        assert!(
            attrs.windows(3).any(|w| w == [0x19, 0x11, 0x34]),
            "expected MAP profile UUID 0x1134 in BluetoothProfileDescriptorList"
        );
    }

    #[test]
    fn sdp_server_with_mns_record_returns_it_for_mns_query() {
        let mut server = SdpServer::new_with_hfp();
        server.register(mns_service_record());
        // Querying for the MNS UUID alone should find exactly the MNS
        // record; it must NOT also surface the HFP record.
        let matched = server.match_records(&[UUID_MAP_MNS_SERVER]);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].handle, AOKIE_MNS_SERVICE_RECORD_HANDLE);
        // And the HFP query still works after the MNS registration.
        let matched_hfp = server.match_records(&[UUID_HANDSFREE]);
        assert_eq!(matched_hfp.len(), 1);
        assert_eq!(matched_hfp[0].handle, AOKIE_HFP_SERVICE_RECORD_HANDLE);
    }

    #[test]
    fn builds_service_search_attribute_response() {
        let record = aokie_hfp_service_record();
        let response = service_search_attribute_response(0x1234, &record);
        assert_eq!(response[0], PDU_SERVICE_SEARCH_ATTRIBUTE_RESPONSE);
        assert_eq!(&response[1..3], &[0x12, 0x34]);
        let param_len = u16::from_be_bytes([response[3], response[4]]) as usize;
        assert_eq!(param_len, response.len() - 5);
        let attr_len = u16::from_be_bytes([response[5], response[6]]) as usize;
        assert_eq!(attr_len, record.len());
        assert_eq!(&response[7..7 + attr_len], &record);
        assert_eq!(response.last().copied(), Some(0));
    }

    #[test]
    fn parses_service_search_attribute_request() {
        let request = [
            0x06, 0x12, 0x34, 0x00, 0x0f, // PDU header, params len 15
            0x35, 0x03, 0x19, 0x11, 0x1e, // service search pattern: Handsfree
            0xff, 0xff, // max attribute bytes
            0x35, 0x05, 0x0a, 0x00, 0x00, 0xff, 0xff, // attr range 0..ffff
            0x00, // continuation state
        ];
        let parsed = parse_service_search_attribute_request(&request).unwrap();
        assert_eq!(parsed.transaction_id, 0x1234);
        assert_eq!(parsed.service_search_pattern, &[0x19, 0x11, 0x1e]);
        assert_eq!(parsed.maximum_attribute_byte_count, 0xffff);
        assert_eq!(parsed.attribute_id_list, &[0x0a, 0x00, 0x00, 0xff, 0xff]);
        assert!(parsed.continuation_state.is_empty());
    }

    #[test]
    fn handles_service_search_attribute_request_with_hfp_record_response() {
        let request = [
            0x06, 0x00, 0x02, 0x00, 0x0f, 0x35, 0x03, 0x19, 0x11, 0x1e, 0xff, 0xff, 0x35, 0x05,
            0x0a, 0x00, 0x00, 0xff, 0xff, 0x00,
        ];
        let response = handle_service_search_attribute_request(&request).unwrap();
        assert_eq!(response[0], PDU_SERVICE_SEARCH_ATTRIBUTE_RESPONSE);
        assert_eq!(&response[1..3], &[0x00, 0x02]);
        assert!(response
            .windows(AOKIE_HFP_SERVICE_NAME.len())
            .any(|w| w == AOKIE_HFP_SERVICE_NAME.as_bytes()));
        assert_eq!(response.last().copied(), Some(0));
    }

    #[test]
    fn rejects_truncated_data_elements() {
        assert!(parse_data_element(&[]).is_err());
        assert!(parse_data_element(&[0x09, 0x12]).is_err());
        assert!(parse_data_element(&[0x35, 0x04, 0x09]).is_err());
        assert!(parse_service_search_attribute_request(&[0x06, 0, 1, 0, 5]).is_err());
    }

    #[test]
    fn sdp_server_returns_hfp_record_when_handsfree_uuid_in_pattern() {
        let server = SdpServer::new_with_hfp();
        // Search pattern: Handsfree (0x111e)
        let request = [
            0x06, 0x00, 0x02, 0x00, 0x0f, 0x35, 0x03, 0x19, 0x11, 0x1e, 0xff, 0xff, 0x35, 0x05,
            0x0a, 0x00, 0x00, 0xff, 0xff, 0x00,
        ];
        let response = server.handle_request(&request).unwrap();
        assert_eq!(response[0], PDU_SERVICE_SEARCH_ATTRIBUTE_RESPONSE);
        assert!(response
            .windows(AOKIE_HFP_SERVICE_NAME.len())
            .any(|w| w == AOKIE_HFP_SERVICE_NAME.as_bytes()));
    }

    #[test]
    fn sdp_server_filters_records_by_service_search_pattern() {
        let mut server = SdpServer::default();
        server.register(hfp_service_record());

        // Made-up second record (think MAP/PBAP) — service-class UUID
        // 0xdead. The HFP query (0x111e) must not return this record.
        const FAKE_PROFILE_UUID: u16 = 0xdead;
        let mut attrs = Vec::new();
        add_attribute(&mut attrs, ATTR_SERVICE_RECORD_HANDLE, |out| {
            encode_data_element(DataElement::Uint32(0xbeef), out);
        });
        // Embed FAKE_PROFILE_UUID as a Uuid16 inside ServiceClassIDList
        // so `ServiceRecord::new` finds it when computing the searchable
        // set. Without this, the fake record's searchable_uuids would be
        // empty and the FakeProfile query below would never match.
        add_attribute(&mut attrs, ATTR_SERVICE_CLASS_ID_LIST, |out| {
            let mut seq = Vec::new();
            encode_data_element(DataElement::Uuid16(FAKE_PROFILE_UUID), &mut seq);
            encode_data_element(DataElement::Sequence(&seq), out);
        });
        add_attribute(&mut attrs, ATTR_SERVICE_NAME, |out| {
            encode_data_element(DataElement::Text("FakeProfile"), out);
        });
        server.register(ServiceRecord::new(0xbeef, attrs));

        // Query for Handsfree only — should match HFP record, not the fake.
        let request = [
            0x06, 0x00, 0x02, 0x00, 0x0f, 0x35, 0x03, 0x19, 0x11, 0x1e, 0xff, 0xff, 0x35, 0x05,
            0x0a, 0x00, 0x00, 0xff, 0xff, 0x00,
        ];
        let response = server.handle_request(&request).unwrap();
        assert!(response
            .windows(AOKIE_HFP_SERVICE_NAME.len())
            .any(|w| w == AOKIE_HFP_SERVICE_NAME.as_bytes()));
        assert!(
            !response.windows(11).any(|w| w == b"FakeProfile"),
            "FakeProfile leaked into HFP-only query"
        );

        // Query for FakeProfile only — should match the fake, not HFP.
        let request_fake = [
            0x06, 0x00, 0x03, 0x00, 0x0f, 0x35, 0x03, 0x19, 0xde, 0xad, 0xff, 0xff, 0x35, 0x05,
            0x0a, 0x00, 0x00, 0xff, 0xff, 0x00,
        ];
        let response_fake = server.handle_request(&request_fake).unwrap();
        assert!(response_fake.windows(11).any(|w| w == b"FakeProfile"));
        assert!(
            !response_fake
                .windows(AOKIE_HFP_SERVICE_NAME.len())
                .any(|w| w == AOKIE_HFP_SERVICE_NAME.as_bytes()),
            "HFP record leaked into FakeProfile-only query"
        );
    }

    #[test]
    fn sdp_server_matches_protocol_uuid_in_search_pattern() {
        // Regression for Pixel 10a / Bluedroid disconnect: phones probe
        // with `[L2CAP]` (0x0100) as a generic "what services do you
        // have?" sweep. SDP §2.5.1 says the matcher must succeed because
        // L2CAP is in every record's ProtocolDescriptorList. The pre-fix
        // matcher only looked at ServiceClassIDList and returned 0
        // matches, after which the phone tore the ACL down.
        let mut server = SdpServer::new_with_hfp();
        server.register(mns_service_record());
        server.register(pnp_device_id_service_record());

        // L2CAP sweep hits records with a ProtocolDescriptorList
        // entry: HFP and MNS. PnP is metadata-only (no transport
        // stack of its own), matching the shape BlueZ uses.
        let matched_l2cap = server.match_records(&[UUID_L2CAP]);
        assert_eq!(matched_l2cap.len(), 2, "L2CAP sweep should hit HFP + MNS");

        // PublicBrowseRoot (0x1002) is the BrowseGroup probe — every
        // record we publish belongs to the public root, so all three
        // come back.
        let matched_browse = server.match_records(&[UUID_PUBLIC_BROWSE_ROOT]);
        assert_eq!(matched_browse.len(), 3);

        // PnP probe (0x1200) — Pixel sends this on first contact. Must
        // resolve to the PnP record specifically.
        let matched_pnp = server.match_records(&[UUID_PNP_INFORMATION]);
        assert_eq!(matched_pnp.len(), 1);
        assert_eq!(matched_pnp[0].handle, AOKIE_PNP_SERVICE_RECORD_HANDLE);

        // HFP-specific probe still uniquely picks the HFP record.
        let matched_hfp = server.match_records(&[UUID_HANDSFREE]);
        assert_eq!(matched_hfp.len(), 1);
        assert_eq!(matched_hfp[0].handle, AOKIE_HFP_SERVICE_RECORD_HANDLE);
    }

    #[test]
    fn pnp_record_advertises_required_attributes() {
        let record = pnp_device_id_service_record();
        assert_eq!(record.handle, AOKIE_PNP_SERVICE_RECORD_HANDLE);
        let mut wrapped = Vec::new();
        encode_data_element(DataElement::Sequence(&record.encoded_record), &mut wrapped);
        let (root, _) = parse_data_element(&wrapped).unwrap();
        let ParsedDataElement::Sequence(attrs) = root else {
            panic!("PnP record root is not a sequence");
        };
        // ServiceClassIDList contains 0x1200.
        assert!(attrs.windows(3).any(|w| w == [0x19, 0x12, 0x00]));
        // PrimaryRecord (0x0204) emitted as Boolean true (0x28, 0x01).
        assert!(attrs.windows(2).any(|w| w == [0x28, 0x01]));
    }

    #[test]
    fn boolean_data_element_round_trips() {
        let mut out = Vec::new();
        encode_data_element(DataElement::Bool(true), &mut out);
        assert_eq!(out, [0x28, 0x01]);
        assert_eq!(
            parse_data_element(&out).unwrap(),
            (ParsedDataElement::Bool(true), 2)
        );
        out.clear();
        encode_data_element(DataElement::Bool(false), &mut out);
        assert_eq!(out, [0x28, 0x00]);
        assert_eq!(
            parse_data_element(&out).unwrap(),
            (ParsedDataElement::Bool(false), 2)
        );
    }

    #[test]
    fn sdp_server_returns_empty_outer_sequence_when_no_records_match() {
        let server = SdpServer::new_with_hfp();
        // Search pattern: 0xfeed — nothing registered matches.
        let request = [
            0x06, 0x00, 0x02, 0x00, 0x0f, 0x35, 0x03, 0x19, 0xfe, 0xed, 0xff, 0xff, 0x35, 0x05,
            0x0a, 0x00, 0x00, 0xff, 0xff, 0x00,
        ];
        let response = server.handle_request(&request).unwrap();
        // Response still well-formed: PDU 0x07, then param_len, then a
        // 16-bit attr_len of 0x0002 covering an empty outer sequence
        // [0x35, 0x00], then continuation 0x00.
        assert_eq!(response[0], PDU_SERVICE_SEARCH_ATTRIBUTE_RESPONSE);
        let attr_len = u16::from_be_bytes([response[5], response[6]]) as usize;
        assert_eq!(attr_len, 2);
        assert_eq!(&response[7..9], &[0x35, 0x00]);
        assert_eq!(response.last().copied(), Some(0));
    }

    #[test]
    fn fuzz_parse_data_element_does_not_panic_on_random_bytes() {
        // Data elements are a recursive type-length-value format.
        // Random bytes must always resolve to Ok or Err — no panic
        // even on absurd nested sequence lengths.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0x5344_5044_4154_4145);
        for _ in 0..5_000 {
            let len = (rng.next_u32() % 512) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = parse_data_element(&buf);
        }
    }

    #[test]
    fn fuzz_parse_service_search_attribute_request_does_not_panic_on_random_bytes() {
        // The server-side request parser accepts SDP PDUs from the
        // peer; malformed PDUs must surface as Err.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0x5344_5052_4551_3030);
        for _ in 0..5_000 {
            let len = (rng.next_u32() % 512) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = parse_service_search_attribute_request(&buf);
        }
    }
}
