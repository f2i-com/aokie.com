//! Pairing + bonded-device helpers for the native backend.
//!
//! Windows owns the actual SSP ceremony (system consent dialog, link keys) —
//! our job is only: list bonds, open a discoverable window (the native
//! pairing window), and forget devices. Device-initiated pairing while the
//! window is open surfaces Windows' own dialog, so there is no
//! `PairingConfirmRequired` flow in native mode.

use windows::Devices::Bluetooth::BluetoothDevice;
use windows::Devices::Enumeration::DeviceInformation;

/// Format a numeric Bluetooth address the same way the dongle backend does
/// (`XX:XX:XX:XX:XX:XX`, uppercase) so `phone.status`/flows see one shape.
pub(crate) fn format_address(address: u64) -> String {
    let b = address.to_be_bytes();
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        b[2], b[3], b[4], b[5], b[6], b[7]
    )
}

/// Parse an `XX:XX:..` (or contiguous-hex) address back to u64. Accepts the
/// dongle backend's canonical form; returns None on anything else.
pub(crate) fn parse_address(text: &str) -> Option<u64> {
    let hex: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 12 {
        return None;
    }
    u64::from_str_radix(&hex, 16).ok()
}

/// Bonded classic-Bluetooth devices as (address, friendly name). Best-effort:
/// an enumeration failure yields an empty list (phone.status degrades softly),
/// never a panic.
pub(crate) fn bonded_devices() -> Vec<(String, Option<String>)> {
    let sel = match BluetoothDevice::GetDeviceSelectorFromPairingState(true) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let infos = match DeviceInformation::FindAllAsyncAqsFilter(&sel).and_then(|op| op.get()) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for info in infos {
        let name = info.Name().ok().map(|n| n.to_string());
        let address = info
            .Id()
            .ok()
            .and_then(|id| BluetoothDevice::FromIdAsync(&id).ok())
            .and_then(|op| op.get().ok())
            .map(|dev| dev.BluetoothAddress().unwrap_or(0))
            .unwrap_or(0);
        if address != 0 {
            out.push((format_address(address), name));
        }
    }
    out
}

/// Forget a bonded device. Returns Ok(true) when Windows reported the device
/// unpaired, Ok(false) when no such bond exists, Err on a real failure.
pub(crate) fn unpair(address: &str) -> Result<bool, String> {
    let wanted = parse_address(address).ok_or_else(|| format!("bad address '{address}'"))?;
    let sel =
        BluetoothDevice::GetDeviceSelectorFromPairingState(true).map_err(|e| e.to_string())?;
    let infos = DeviceInformation::FindAllAsyncAqsFilter(&sel)
        .and_then(|op| op.get())
        .map_err(|e| e.to_string())?;
    for info in infos {
        let id = info.Id().map_err(|e| e.to_string())?;
        let addr = BluetoothDevice::FromIdAsync(&id)
            .and_then(|op| op.get())
            .map(|dev| dev.BluetoothAddress().unwrap_or(0))
            .unwrap_or(0);
        if addr == wanted {
            let status = info
                .Pairing()
                .map_err(|e| e.to_string())?
                .UnpairAsync()
                .map_err(|e| e.to_string())?
                .get()
                .map_err(|e| e.to_string())?
                .Status()
                .map_err(|e| e.to_string())?;
            return Ok(matches!(
                status,
                windows::Devices::Enumeration::DeviceUnpairingResultStatus::Unpaired
                    | windows::Devices::Enumeration::DeviceUnpairingResultStatus::AlreadyUnpaired
            ));
        }
    }
    Ok(false)
}

/// Toggle classic Bluetooth discoverability on the first local radio — the
/// native "pairing window". Windows only advertises the PC while this is on
/// (Settings keeps it off by default). Callers re-disable on window close.
#[cfg(target_os = "windows")]
pub(crate) fn set_discoverable(enabled: bool) -> Result<(), String> {
    use windows::Win32::Devices::Bluetooth::{
        BluetoothEnableDiscovery, BluetoothFindFirstRadio, BluetoothFindRadioClose,
        BLUETOOTH_FIND_RADIO_PARAMS,
    };
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    unsafe {
        let params = BLUETOOTH_FIND_RADIO_PARAMS {
            dwSize: std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32,
        };
        let mut radio = HANDLE::default();
        // Err (ERROR_NO_MORE_ITEMS) = no local radio.
        let search = BluetoothFindFirstRadio(&params, &mut radio)
            .map_err(|e| format!("BluetoothFindFirstRadio: {e}"))?;
        let ok = BluetoothEnableDiscovery(Some(radio), enabled);
        let _ = BluetoothFindRadioClose(search);
        let _ = CloseHandle(radio);
        if ok.as_bool() {
            Ok(())
        } else {
            Err("BluetoothEnableDiscovery refused".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_round_trips_dongle_style() {
        assert_eq!(format_address(0x04C8B0E13FF3), "04:C8:B0:E1:3F:F3");
        assert_eq!(parse_address("04:C8:B0:E1:3F:F3"), Some(0x04C8B0E13FF3));
        assert_eq!(parse_address("04c8b0e13ff3"), Some(0x04C8B0E13FF3));
        assert_eq!(parse_address("not an address"), None);
        assert_eq!(parse_address("04:C8:B0"), None);
    }
}
