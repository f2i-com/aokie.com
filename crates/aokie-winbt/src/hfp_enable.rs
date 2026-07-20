//! Query + enable a paired device's classic profile services via the Win32
//! Bluetooth APIs (`BluetoothEnumerateInstalledServices`,
//! `BluetoothSetServiceState`) — the same switches the Services tab in
//! Devices and Printers shows. Used to force "Hands-free Telephony" on when
//! a pairing negotiated A2DP-only.

use windows::core::GUID;
use windows::Win32::Devices::Bluetooth::{
    BluetoothEnumerateInstalledServices, BluetoothFindDeviceClose, BluetoothFindFirstDevice,
    BluetoothFindNextDevice, BluetoothFindFirstRadio, BluetoothFindRadioClose,
    BluetoothSetServiceState, BLUETOOTH_DEVICE_INFO, BLUETOOTH_DEVICE_SEARCH_PARAMS,
    BLUETOOTH_FIND_RADIO_PARAMS, BLUETOOTH_SERVICE_ENABLE,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};

struct RadioGuard(HANDLE);
impl Drop for RadioGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Base SDP UUID (00000000-0000-1000-8000-00805F9B34FB) with a uuid16 in the
/// high 32 bits.
fn guid_from_uuid16(uuid: u16) -> GUID {
    GUID::from_u128(0x0000_1000_8000_0080_5F9B_34FB | ((uuid as u128) << 96))
}

pub fn run(phone_address: u64) {
    unsafe {
        let mut radio = HANDLE::default();
        let params = BLUETOOTH_FIND_RADIO_PARAMS {
            dwSize: std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32,
        };
        let search = match BluetoothFindFirstRadio(&params, &mut radio) {
            Ok(s) => s,
            Err(e) => {
                println!("no radio: {e}");
                return;
            }
        };
        let _guard = RadioGuard(radio);
        let _search_guard = scopeguard_radio_search(search);

        let mut sp = BLUETOOTH_DEVICE_SEARCH_PARAMS {
            dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
            ..Default::default()
        };
        sp.fReturnAuthenticated = true.into();
        sp.fReturnConnected = true.into();

        let mut info = BLUETOOTH_DEVICE_INFO {
            dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
            ..Default::default()
        };
        let mut found = false;
        if let Ok(find) = BluetoothFindFirstDevice(&sp, &mut info) {
            loop {
                if info.Address.Anonymous.ullLong == phone_address {
                    found = true;
                    break;
                }
                if BluetoothFindNextDevice(find, &mut info).is_err() {
                    break;
                }
            }
            let _ = BluetoothFindDeviceClose(find);
        }
        if !found {
            println!("phone {phone_address:012X} not found by BluetoothFindFirstDevice");
            return;
        }
        let name = String::from_utf16_lossy(&info.szName)
            .trim_end_matches('\0')
            .to_string();
        println!(
            "device: {name} connected={} authenticated={} remembered={}",
            info.fConnected.as_bool(),
            info.fAuthenticated.as_bool(),
            info.fRemembered.as_bool()
        );

        let mut count = 0u32;
        let rc = BluetoothEnumerateInstalledServices(
            Some(radio),
            &info,
            &mut count,
            None,
        );
        println!("installed services: {count} (first enum rc={rc})");
        let mut guids = vec![GUID::default(); count.max(1) as usize];
        let rc = BluetoothEnumerateInstalledServices(
            Some(radio),
            &info,
            &mut count,
            Some(guids.as_mut_ptr()),
        );
        if rc != 0 {
            println!("BluetoothEnumerateInstalledServices failed: Win32 error {rc}");
            return;
        }
        for g in &guids[..count.min(guids.len() as u32) as usize] {
            let uuid16 = (g.to_u128() >> 96) as u16;
            println!("  service uuid16=0x{uuid16:04X}");
        }

        // ENABLE the phone's HFP Audio Gateway (0x111F) and the headset
        // gateway service (0x1112) — the Services-tab switches.
        for uuid in [0x111Fu16, 0x1112u16] {
            let g = guid_from_uuid16(uuid);
            let rc = BluetoothSetServiceState(Some(radio), &info, &g, BLUETOOTH_SERVICE_ENABLE);
            println!(
                "BluetoothSetServiceState(0x{uuid:04X}, ENABLE) -> {rc} (0 = ok, 1213 = not found?)"
            );
        }
    }
}

fn scopeguard_radio_search(search: windows::Win32::Devices::Bluetooth::HBLUETOOTH_RADIO_FIND) -> impl Drop {
    struct G(windows::Win32::Devices::Bluetooth::HBLUETOOTH_RADIO_FIND);
    impl Drop for G {
        fn drop(&mut self) {
            unsafe {
                let _ = BluetoothFindRadioClose(self.0);
            }
        }
    }
    G(search)
}
