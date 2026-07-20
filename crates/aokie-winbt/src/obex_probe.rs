//! Raw OBEX CONNECT probe: distinguishes "the RFCOMM link itself is broken"
//! from "this specific profile refuses us" by sending a bare OBEX CONNECT to
//! the phone's PBAP PSE and MAP MAS and printing the response (or the way the
//! channel died). Diagnostic only.

use aokie_bluetooth::aokie_radio::map_mas::MAS_TARGET_UUID;
use aokie_bluetooth::aokie_radio::obex::build_connect_request;
use aokie_bluetooth::aokie_radio::pbap::{DEFAULT_MAX_PACKET_LENGTH, PBAP_TARGET_UUID};

use crate::rfcomm::BtChannel;

fn probe_one(label: &str, phone_address: u64, short_uuid: u16, target: &[u8]) {
    println!("--- {label} (uuid 0x{short_uuid:04X}) ---");
    let chan = match BtChannel::connect(phone_address, short_uuid) {
        Ok(c) => c,
        Err(e) => {
            println!("  RFCOMM connect failed: {e}");
            return;
        }
    };
    println!("  RFCOMM channel open");
    let req = build_connect_request(Some(target), DEFAULT_MAX_PACKET_LENGTH);
    if let Err(e) = chan.write_all(&req) {
        println!("  write failed: {e}");
        return;
    }
    println!("  OBEX CONNECT sent ({} bytes)", req.len());
    let mut buf = [0u8; 64];
    match chan.read_some(&mut buf) {
        Ok(n) => {
            let rsp = buf[0];
            let meaning = match rsp {
                0xA0 => "SUCCESS — the profile accepts us",
                0xC1 => "UNAUTHORIZED",
                0xC3 => "FORBIDDEN — permission/profile refused",
                0xC6 => "NOT ACCEPTABLE",
                0xD3 => "SERVICE UNAVAILABLE",
                other => "other response",
            };
            println!(
                "  response: 0x{rsp:02X} ({meaning}), {} bytes: {:02X?}",
                n,
                &buf[..n.min(24)]
            );
        }
        Err(e) => println!("  read failed: {e}"),
    }
}

pub fn run(phone_address: u64) {
    probe_one("PBAP PSE", phone_address, 0x112F, &PBAP_TARGET_UUID);
    probe_one("MAP MAS", phone_address, 0x1132, &MAS_TARGET_UUID);
}
