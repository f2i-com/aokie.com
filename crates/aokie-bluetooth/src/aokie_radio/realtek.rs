//! Realtek controllers run their ROM until their firmware is loaded into them, at every start: an RTL8761B dongle
//! (`0bda:8771`, many TP-Link and ASUS dongles), or the Bluetooth half of an RTL8822BU Wi-Fi and Bluetooth adapter
//! (`0bda:b82c`, "802.11ac NIC"), on its ROM alone takes Aokie's name, class and scan settings and then is not seen
//! by a phone at all ("I can't seem to see Aokie AI Assistant when pairing the phone"). Windows' own
//! driver did not start one either: Realtek's driver loads the firmware, and Aokie replaces that driver. This is the
//! loader Linux's `drivers/bluetooth/btrtl.c` is, ported for the chips listed in [`CHIPS`]: the controller's ROM
//! version read (`0xfc6d`), the patch for it taken from Realtek's firmware file (its "Realtech" container), the
//! firmware's version written over the patch's last four bytes, the config file appended, and the whole sent in
//! pieces of 252 bytes (`0xfc20`). It lives in the controller's RAM: an HCI reset keeps it, a replug clears it.
//!
//! The files are Realtek's, as linux-firmware redistributes them (`rtl_bt/rtl8761bu_fw.bin` and
//! `rtl_bt/rtl8761bu_config.bin`, `rtl_bt/rtl8822b_fw.bin` and `rtl_bt/rtl8822b_config.bin`, under
//! `LICENCE.rtlwifi_firmware.txt`): looked for in [`firmware_dirs`]. Without
//! them the controller runs its ROM as before, and the log says what is missing.

use std::path::PathBuf;

use crate::aokie_radio::hci::LocalVersion;
use crate::aokie_radio::transport::AokieHciTransport;

/// Bluetooth SIG company ID for Realtek Semiconductor (`Read_Local_Version_Information`'s `manufacturer_name`).
pub const BLUETOOTH_COMPANY_ID_REALTEK: u16 = 0x005d;
/// Realtek's vendor command: the ROM's version (its return: status, version).
pub const OPCODE_RTL_READ_ROM_VERSION: u16 = 0xfc6d;
/// Realtek's vendor command: a piece of the firmware (its parameters: index, then up to [`FRAG_LEN`] bytes; its
/// return: status, index).
pub const OPCODE_RTL_DOWNLOAD: u16 = 0xfc20;
/// The bytes of a download piece (btrtl.h's `RTL_FRAG_LEN`).
pub const FRAG_LEN: usize = 252;

const EPATCH_SIGNATURE: &[u8; 8] = b"Realtech";
const EPATCH_SIGNATURE_V2: &[u8; 8] = b"RTBTCore";
const EXTENSION_SIGNATURE: [u8; 4] = [0x51, 0x04, 0xfd, 0x77];
/// The "Realtech" container's header: its signature, the firmware's version (le32), the patches' number (le16).
const EPATCH_HEADER_LEN: usize = 8 + 4 + 2;

/// A Realtek controller as its ROM reports itself, and its firmware's files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtlChip {
    pub name: &'static str,
    pub lmp_subversion: u16,
    pub hci_revision: u16,
    pub hci_version: u8,
    pub firmware: &'static str,
    pub config: &'static str,
}

/// The chips whose firmware is loaded here, as btrtl.c's `ic_id_table` has them (on USB). Others run as they did.
pub const CHIPS: &[RtlChip] = &[
    RtlChip {
        name: "RTL8761BU",
        lmp_subversion: 0x8761,
        hci_revision: 0x000b,
        hci_version: 0x0a,
        firmware: "rtl8761bu_fw.bin",
        config: "rtl8761bu_config.bin",
    },
    // (the Bluetooth half of a Wi-Fi and Bluetooth adapter; its ROM says Bluetooth 4.1, its firmware more)
    RtlChip {
        name: "RTL8822BU",
        lmp_subversion: 0x8822,
        hci_revision: 0x000b,
        hci_version: 0x07,
        firmware: "rtl8822b_fw.bin",
        config: "rtl8822b_config.bin",
    },
];

/// The project ids a "Realtech" container's trailer names, and the ROM's LMP subversion each is for (btrtl.c's
/// `project_id_to_lmp_subver`).
const PROJECTS: &[(u8, u16)] = &[
    (0, 0x1200),
    (1, 0x8723),
    (2, 0x8821),
    (3, 0x8761),
    (7, 0x8703),
    (8, 0x8822),
    (9, 0x8723),
    (10, 0x8821),
    (13, 0x8822),
    (14, 0x8761),
    (18, 0x8852),
    (20, 0x8852),
    (25, 0x8852),
    (36, 0x8851),
    (44, 0x8922),
    (47, 0x8852),
    (51, 0x8761),
];

/// The chip of [`CHIPS`] whose ROM this controller is running, if it is one (a controller with its firmware loaded
/// reports the firmware's subversion instead, and is left as it is).
pub fn rom_chip(version: &LocalVersion) -> Option<&'static RtlChip> {
    if version.manufacturer_name != BLUETOOTH_COMPANY_ID_REALTEK {
        return None;
    }
    CHIPS.iter().find(|c| c.lmp_subversion == version.lmp_pal_subversion && c.hci_revision == version.hci_revision && c.hci_version == version.hci_version)
}

fn le16(b: &[u8], at: usize) -> Result<u16, String> {
    b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or_else(|| "the firmware file ends too soon".to_string())
}

fn le32(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| "the firmware file ends too soon".to_string())
}

/// The project id a "Realtech" container's trailer names: its instructions read backwards from before the extension
/// signature at the file's end, as btrtl.c's `rtlbt_parse_firmware` reads them.
pub fn project_id(fw: &[u8]) -> Result<u8, String> {
    if fw.len() < EPATCH_HEADER_LEN + EXTENSION_SIGNATURE.len() + 3 {
        return Err("the firmware file is too short".into());
    }
    if fw[fw.len() - 4..] != EXTENSION_SIGNATURE {
        return Err("the firmware file's extension signature is not Realtek's".into());
    }
    let mut at = fw.len() - 4;
    while at >= EPATCH_HEADER_LEN + 3 {
        let (opcode, length, data) = (fw[at - 1], fw[at - 2], fw[at - 3]);
        at -= 3;
        if opcode == 0xff {
            break;
        }
        if length == 0 {
            return Err("the firmware file has an instruction of length 0".into());
        }
        if opcode == 0 && length == 1 {
            return Ok(data);
        }
        at = at.checked_sub(length as usize).ok_or("the firmware file's trailer runs past its start")?;
    }
    Err("the firmware file names no project".into())
}

/// The patch for a controller whose ROM is `lmp_subversion` at ROM version `rom_version`, out of a "Realtech"
/// container: the one whose chip id is the ROM version plus one, the firmware's version written over its last four
/// bytes (btrtl.c's `rtlbt_parse_firmware`). The newer "RTBTCore" container is refused (no chip of [`CHIPS`] uses it).
pub fn patch_for(fw: &[u8], lmp_subversion: u16, rom_version: u8) -> Result<Vec<u8>, String> {
    if fw.len() <= 8 {
        return Err("the firmware file is too short".into());
    }
    if &fw[..8] == EPATCH_SIGNATURE_V2 {
        return Err("the firmware file is Realtek's newer (RTBTCore) kind, which Aokie does not load".into());
    }
    if &fw[..8] != EPATCH_SIGNATURE {
        return Err("the firmware file is not a Realtek patch (no Realtech signature)".into());
    }
    let project = project_id(fw)?;
    match PROJECTS.iter().find(|(id, _)| *id == project) {
        None => return Err(format!("the firmware file is for project {project}, which Aokie does not know")),
        Some((_, for_subversion)) if *for_subversion != lmp_subversion => {
            return Err(format!("the firmware file is for a 0x{for_subversion:04x} controller, and this one is 0x{lmp_subversion:04x}"));
        }
        Some(_) => {}
    }
    let fw_version = le32(fw, 8)?;
    let patches = le16(fw, 12)? as usize;
    if fw.len() < EPATCH_HEADER_LEN + EXTENSION_SIGNATURE.len() + 3 + 8 * patches {
        return Err("the firmware file is too short for its patches".into());
    }
    let chip_ids = EPATCH_HEADER_LEN;
    let lengths = chip_ids + 2 * patches;
    let offsets = lengths + 2 * patches;
    let want = rom_version as u16 + 1;
    let mut found = None;
    for i in 0..patches {
        if le16(fw, chip_ids + 2 * i)? == want {
            found = Some((le16(fw, lengths + 2 * i)? as usize, le32(fw, offsets + 4 * i)? as usize));
            break;
        }
    }
    let (length, offset) = match found {
        Some((length, offset)) if offset != 0 => (length, offset),
        _ => return Err(format!("the firmware file has no patch for ROM version {rom_version}")),
    };
    if length < 4 || offset > fw.len() || length > fw.len() - offset {
        return Err("the firmware file's patch lies past its end".into());
    }
    let mut patch = fw[offset..offset + length].to_vec();
    patch[length - 4..].copy_from_slice(&fw_version.to_le_bytes());
    Ok(patch)
}

/// The pieces a download sends `image` in, each with its index (btrtl.c's `rtl_download_firmware`): 252 bytes each,
/// one more than the whole pieces (the last may hold none), indices counting from 0 to 0x7f and then from 1 again, the
/// last's index with 0x80 set (the end).
pub fn pieces(image: &[u8]) -> Vec<(u8, &[u8])> {
    let count = image.len() / FRAG_LEN + 1;
    let mut out = Vec::with_capacity(count);
    let mut next: u8 = 0;
    for i in 0..count {
        let mut index = next;
        next = if index == 0x7f { 1 } else { next + 1 };
        let start = i * FRAG_LEN;
        let end = if i == count - 1 {
            index |= 0x80;
            start + image.len() % FRAG_LEN
        } else {
            start + FRAG_LEN
        };
        out.push((index, &image[start..end]));
    }
    out
}

/// The download command for one piece: its opcode, its length, the index, the bytes.
pub fn download_command(index: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&OPCODE_RTL_DOWNLOAD.to_le_bytes());
    out.push((1 + data.len()) as u8);
    out.push(index);
    out.extend_from_slice(data);
    out
}

/// Where Realtek's firmware files are looked for: `AOKIE_FIRMWARE_DIR`, the plugin's own `firmware` folder (beside
/// its program, as its bundle has them), and on Linux the system's.
pub fn firmware_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("AOKIE_FIRMWARE_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        dirs.push(exe_dir.join("firmware").join("rtl_bt"));
        dirs.push(exe_dir.join("firmware"));
    }
    if cfg!(target_os = "linux") {
        dirs.push(PathBuf::from("/lib/firmware/rtl_bt"));
        dirs.push(PathBuf::from("/usr/lib/firmware/rtl_bt"));
    }
    dirs
}

/// The firmware and config files of `chip`, from the first of [`firmware_dirs`] that holds both.
fn read_files(chip: &RtlChip) -> Option<(PathBuf, Vec<u8>, Vec<u8>)> {
    firmware_dirs().into_iter().find_map(|dir| {
        let fw = std::fs::read(dir.join(chip.firmware)).ok()?;
        let config = std::fs::read(dir.join(chip.config)).ok()?;
        Some((dir, fw, config))
    })
}

/// What became of a Realtek controller's firmware at the radio's last start, for the dongle screen
/// (`dongle.list`'s `firmware`): a dongle left on its ROM is seen by no phone, and the log alone said why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FirmwareStatus {
    /// The chip, as [`CHIPS`] names it.
    pub chip: &'static str,
    /// `loaded`; `missing` (its files are not where they are looked for); `failed` (the controller refused it).
    /// Either of the last two leaves the controller on its ROM.
    pub state: &'static str,
    /// What was looked for where, or the controller's refusal.
    pub detail: String,
}

static LAST_STATUS: std::sync::Mutex<Option<FirmwareStatus>> = std::sync::Mutex::new(None);

/// The firmware's state at the radio's last start: None when the controller was no Realtek ROM of [`CHIPS`]
/// (another chip, or one whose firmware was loaded already).
pub fn firmware_status() -> Option<FirmwareStatus> {
    LAST_STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_status(status: Option<FirmwareStatus>) {
    *LAST_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = status;
}

/// Load the firmware into a controller running the ROM of a chip of [`CHIPS`]: its version as the firmware reports
/// it, after a reset (which keeps the firmware, and clears the event mask: set it again). None where there is nothing
/// to load (another controller, one whose firmware is loaded, or no files, which the log says). An error where the
/// controller refused a step: the caller goes on with the ROM. Either way [`firmware_status`] says what happened.
pub fn load_firmware(transport: &AokieHciTransport, version: &LocalVersion) -> Result<Option<LocalVersion>, String> {
    let Some(chip) = rom_chip(version) else {
        set_status(None);
        return Ok(None);
    };
    let result = load_chip_firmware(transport, chip);
    set_status(Some(match &result {
        Ok(Some(_)) => FirmwareStatus { chip: chip.name, state: "loaded", detail: String::new() },
        Ok(None) => FirmwareStatus {
            chip: chip.name,
            state: "missing",
            detail: format!(
                "{} and {} were looked for in {} and are not there",
                chip.firmware,
                chip.config,
                firmware_dirs().iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")
            ),
        },
        Err(e) => FirmwareStatus { chip: chip.name, state: "failed", detail: e.clone() },
    }));
    result
}

fn load_chip_firmware(transport: &AokieHciTransport, chip: &'static RtlChip) -> Result<Option<LocalVersion>, String> {
    let Some((dir, fw, config)) = read_files(chip) else {
        let looked: Vec<String> = firmware_dirs().iter().map(|d| d.display().to_string()).collect();
        eprintln!(
            "[AokieRadio] Realtek {} is running its ROM, which a phone does not see: its firmware ({} and {}, Realtek's, as linux-firmware's rtl_bt has them) was looked for in {} and is not there",
            chip.name,
            chip.firmware,
            chip.config,
            looked.join(", ")
        );
        return Ok(None);
    };
    let rom = transport.command_return_params(&[0x6d, 0xfc, 0x00], OPCODE_RTL_READ_ROM_VERSION).map_err(|e| format!("Realtek Read ROM Version: {e}"))?;
    let rom_version = match rom.as_slice() {
        [0, v, ..] => *v,
        [status, ..] => return Err(format!("Realtek Read ROM Version: status 0x{status:02x}")),
        [] => return Err("Realtek Read ROM Version: no answer".into()),
    };
    let mut image = patch_for(&fw, chip.lmp_subversion, rom_version)?;
    image.extend_from_slice(&config);
    let pieces = pieces(&image);
    eprintln!(
        "[AokieRadio] Realtek {} on its ROM (version {rom_version}): loading its firmware from {} ({} bytes with its config, {} pieces)",
        chip.name,
        dir.display(),
        image.len(),
        pieces.len()
    );
    for (n, (index, data)) in pieces.iter().enumerate() {
        let params = transport
            .command_return_params(&download_command(*index, data), OPCODE_RTL_DOWNLOAD)
            .map_err(|e| format!("Realtek firmware download, piece {} of {} ({} bytes): {e}", n + 1, pieces.len(), data.len()))?;
        match params.first() {
            Some(0) => {}
            Some(status) => return Err(format!("Realtek firmware download: piece {} of {} refused, status 0x{status:02x}", n + 1, pieces.len())),
            None => return Err(format!("Realtek firmware download: piece {} of {} not answered", n + 1, pieces.len())),
        }
    }
    transport.reset()?;
    let loaded = transport.read_local_version()?;
    eprintln!(
        "[AokieRadio] Realtek {} firmware loaded: subversion 0x{:04x} where its ROM's is 0x{:04x}, HCI revision 0x{:04x}",
        chip.name, loaded.lmp_pal_subversion, chip.lmp_subversion, loaded.hci_revision
    );
    Ok(Some(loaded))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A "Realtech" container of two patches (chip ids 1 and 2), and a trailer naming `project`.
    fn container(project: u8, patches: &[(u16, &[u8])]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(EPATCH_SIGNATURE);
        f.extend_from_slice(&0xdfc6_d922u32.to_le_bytes());
        f.extend_from_slice(&(patches.len() as u16).to_le_bytes());
        let table = EPATCH_HEADER_LEN + 8 * patches.len();
        for (chip, _) in patches {
            f.extend_from_slice(&chip.to_le_bytes());
        }
        for (_, bytes) in patches {
            f.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        }
        let mut offset = table;
        for (_, bytes) in patches {
            f.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += bytes.len();
        }
        for (_, bytes) in patches {
            f.extend_from_slice(bytes);
        }
        // the trailer, read from its end: an instruction of another kind first (opcode 5, of length 2: a data byte and
        // two more, as btrtl.c skips them), then the project's (opcode 0, length 1), then the signature
        f.extend_from_slice(&[project, 0x01, 0x00]);
        f.extend_from_slice(&[0xa1, 0xa2, 0xbb, 0x02, 0x05]);
        f.extend_from_slice(&EXTENSION_SIGNATURE);
        f
    }

    #[test]
    fn the_patch_for_the_roms_version_is_taken_with_the_firmwares_version_at_its_end() {
        let first: &[u8] = &[1, 1, 1, 1, 1, 1, 1, 1];
        let second: &[u8] = &[2, 2, 2, 2, 2, 2, 2, 2, 2, 2];
        let f = container(14, &[(1, first), (2, second)]);
        assert_eq!(project_id(&f), Ok(14));
        // ROM version 1: chip id 2, the second patch, its last four bytes the firmware's version
        let patch = patch_for(&f, 0x8761, 1).unwrap();
        assert_eq!(patch, [2, 2, 2, 2, 2, 2, 0x22, 0xd9, 0xc6, 0xdf]);
        assert_eq!(&patch_for(&f, 0x8761, 0).unwrap()[..4], &[1, 1, 1, 1]);
        // no patch for that ROM, another chip's file, not a Realtek file, the newer kind
        assert!(patch_for(&f, 0x8761, 7).unwrap_err().contains("no patch for ROM version 7"));
        assert!(patch_for(&f, 0x8822, 1).unwrap_err().contains("0x8761 controller"));
        let mut other = f.clone();
        other[..8].copy_from_slice(b"Notatall");
        assert!(patch_for(&other, 0x8761, 1).is_err());
        other[..8].copy_from_slice(EPATCH_SIGNATURE_V2);
        assert!(patch_for(&other, 0x8761, 1).unwrap_err().contains("RTBTCore"));
        // a trailer that is not Realtek's, and a file cut short
        let mut bad = f.clone();
        let n = bad.len();
        bad[n - 1] = 0;
        assert!(project_id(&bad).is_err());
        assert!(patch_for(&f[..20], 0x8761, 1).is_err());
    }

    #[test]
    fn a_download_is_pieces_of_252_bytes_their_indices_wrapping_after_0x7f_and_the_last_marked() {
        let image: Vec<u8> = (0..(FRAG_LEN * 130 + 7)).map(|i| i as u8).collect();
        let p = pieces(&image);
        assert_eq!(p.len(), 131);
        assert_eq!((p[0].0, p[0].1.len()), (0, FRAG_LEN));
        assert_eq!(p[127].0, 0x7f);
        assert_eq!(p[128].0, 1, "after 0x7f the count goes on from 1");
        assert_eq!(p[129].0, 2);
        assert_eq!((p[130].0, p[130].1.len()), (3 | 0x80, 7), "the last: its index marked, the rest of the bytes");
        assert_eq!(p.iter().map(|(_, d)| d.len()).sum::<usize>(), image.len());
        // a whole number of pieces: one more, empty, marked the end (as btrtl.c sends it)
        let whole = vec![0u8; FRAG_LEN * 2];
        let p = pieces(&whole);
        assert_eq!(p.iter().map(|(i, d)| (*i, d.len())).collect::<Vec<_>>(), vec![(0, FRAG_LEN), (1, FRAG_LEN), (0x82, 0)]);
        assert_eq!(download_command(0x82, &[]), vec![0x20, 0xfc, 1, 0x82]);
        assert_eq!(download_command(5, &[9, 8]), vec![0x20, 0xfc, 3, 5, 9, 8]);
    }

    #[test]
    fn only_a_realtek_rom_of_the_list_is_loaded() {
        let rom = LocalVersion { hci_version: 0x0a, hci_revision: 0x000b, lmp_pal_version: 0x0a, manufacturer_name: 93, lmp_pal_subversion: 0x8761 };
        assert_eq!(rom_chip(&rom).map(|c| c.name), Some("RTL8761BU"));
        // its firmware loaded (another subversion), a Broadcom, another revision
        assert!(rom_chip(&LocalVersion { lmp_pal_subversion: 0xdfc6, ..rom }).is_none());
        assert!(rom_chip(&LocalVersion { manufacturer_name: 0x000f, ..rom }).is_none());
        assert!(rom_chip(&LocalVersion { hci_revision: 0x000a, ..rom }).is_none());
        // an RTL8822BU adapter's Bluetooth on its ROM, as one answered (Bluetooth 4.1, revision 0x000b)
        let rtl8822b = LocalVersion { hci_version: 0x07, hci_revision: 0x000b, lmp_pal_version: 0x07, manufacturer_name: 93, lmp_pal_subversion: 0x8822 };
        assert_eq!(rom_chip(&rtl8822b).map(|c| (c.name, c.firmware, c.config)), Some(("RTL8822BU", "rtl8822b_fw.bin", "rtl8822b_config.bin")));
    }

    /// Realtek's own file, where it is at hand (`AOKIE_FIRMWARE_DIR`): its project is the RTL8761B's, and its ROM
    /// version 1's patch is there.
    #[test]
    fn realteks_rtl8761bu_file_where_it_is_given() {
        let Some(dir) = std::env::var_os("AOKIE_FIRMWARE_DIR") else { return };
        let Ok(fw) = std::fs::read(PathBuf::from(dir).join("rtl8761bu_fw.bin")) else { return };
        assert_eq!(project_id(&fw), Ok(14));
        for rom in [0u8, 1] {
            if let Ok(patch) = patch_for(&fw, 0x8761, rom) {
                assert_eq!(&patch[patch.len() - 4..], &fw[8..12]);
                return;
            }
        }
        panic!("no patch for ROM version 0 or 1 in Realtek's file");
    }

    /// Realtek's RTL8822B file, where it is at hand (`AOKIE_FIRMWARE_DIR`): its project is the 0x8822's (8), it is
    /// not the RTL8761B's, and it has a patch for each of ROM versions 0 to 2.
    #[test]
    fn realteks_rtl8822b_file_where_it_is_given() {
        let Some(dir) = std::env::var_os("AOKIE_FIRMWARE_DIR") else { return };
        let Ok(fw) = std::fs::read(PathBuf::from(dir).join("rtl8822b_fw.bin")) else { return };
        assert_eq!(project_id(&fw), Ok(8));
        assert!(patch_for(&fw, 0x8761, 1).unwrap_err().contains("0x8822 controller"));
        for rom in [0u8, 1, 2] {
            let patch = patch_for(&fw, 0x8822, rom).unwrap();
            assert_eq!(&patch[patch.len() - 4..], &fw[8..12], "ROM version {rom}");
        }
    }
}
