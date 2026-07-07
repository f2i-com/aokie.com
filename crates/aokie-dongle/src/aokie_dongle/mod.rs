#![cfg(target_os = "windows")]

use std::mem::{size_of, zeroed};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW, DIGCF_ALLCLASSES,
    DIGCF_PRESENT, HDEVINFO, SPDRP_CLASS, SPDRP_COMPATIBLEIDS, SPDRP_DEVICEDESC,
    SPDRP_FRIENDLYNAME, SPDRP_HARDWAREID, SPDRP_SERVICE, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Registry::{REG_EXPAND_SZ, REG_MULTI_SZ, REG_SZ};

pub mod installer;
pub mod pki;
pub mod winusb;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UsbDevice {
    pub vid: u16,
    pub pid: u16,
    pub description: String,
    pub driver: String,
    pub hardware_id: String,
    pub is_composite: bool,
}

pub fn list_devices(list_all: bool) -> Result<Vec<UsbDevice>, String> {
    unsafe { list_devices_impl(list_all) }
}

unsafe fn list_devices_impl(list_all: bool) -> Result<Vec<UsbDevice>, String> {
    let enumerator = wide_null("USB");
    let info_set = SetupDiGetClassDevsW(
        null(),
        enumerator.as_ptr(),
        null_mut(),
        DIGCF_PRESENT | DIGCF_ALLCLASSES,
    );
    if info_set == INVALID_HANDLE_VALUE as HDEVINFO {
        return Err(format!(
            "SetupDiGetClassDevsW(USB) failed: Win32 error {}",
            GetLastError()
        ));
    }

    let info_set = DeviceInfoSet(info_set);
    let mut devices = Vec::new();
    let mut index = 0;

    loop {
        let mut info = zeroed::<SP_DEVINFO_DATA>();
        info.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;

        if SetupDiEnumDeviceInfo(info_set.0, index, &mut info) == 0 {
            let err = GetLastError();
            if err == ERROR_NO_MORE_ITEMS {
                break;
            }
            return Err(format!(
                "SetupDiEnumDeviceInfo({}) failed: Win32 error {}",
                index, err
            ));
        }
        index += 1;

        let instance_id = get_device_instance_id(info_set.0, &info).unwrap_or_default();
        let hardware_ids = get_property_strings(info_set.0, &info, SPDRP_HARDWAREID);
        let compatible_ids = get_property_strings(info_set.0, &info, SPDRP_COMPATIBLEIDS);

        let vid_pid = std::iter::once(instance_id.as_str())
            .chain(hardware_ids.iter().map(String::as_str))
            .find_map(parse_vid_pid);
        let Some((vid, pid)) = vid_pid else {
            continue;
        };

        let driver = normalize_driver_name(
            &get_property_string(info_set.0, &info, SPDRP_SERVICE).unwrap_or_default(),
        );
        if !list_all && !driver.is_empty() {
            continue;
        }

        let class = get_property_string(info_set.0, &info, SPDRP_CLASS).unwrap_or_default();
        let description = get_property_string(info_set.0, &info, SPDRP_FRIENDLYNAME)
            .or_else(|| get_property_string(info_set.0, &info, SPDRP_DEVICEDESC))
            .unwrap_or_else(|| {
                first_non_empty(&hardware_ids).unwrap_or_else(|| instance_id.clone())
            });

        if is_hub_or_controller(&description, &driver, &class) {
            continue;
        }

        let hardware_id = first_non_empty(&hardware_ids).unwrap_or_else(|| instance_id.clone());
        let is_composite = is_composite_device(&hardware_ids, &compatible_ids, &driver);

        devices.push(UsbDevice {
            vid,
            pid,
            description: description.trim().to_string(),
            driver,
            hardware_id,
            is_composite,
        });
    }

    Ok(devices)
}

struct DeviceInfoSet(HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

unsafe fn get_device_instance_id(info_set: HDEVINFO, info: &SP_DEVINFO_DATA) -> Option<String> {
    let mut required = 0;
    let ok = SetupDiGetDeviceInstanceIdW(info_set, info, null_mut(), 0, &mut required);
    if ok == 0 && GetLastError() != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }
    if required == 0 {
        return None;
    }

    let mut buffer = vec![0u16; required as usize];
    if SetupDiGetDeviceInstanceIdW(
        info_set,
        info,
        buffer.as_mut_ptr(),
        buffer.len() as u32,
        &mut required,
    ) == 0
    {
        return None;
    }

    let id = utf16z_to_string(&buffer);
    (!id.is_empty()).then_some(id)
}

unsafe fn get_property_string(
    info_set: HDEVINFO,
    info: &SP_DEVINFO_DATA,
    property: u32,
) -> Option<String> {
    get_property_strings(info_set, info, property)
        .into_iter()
        .next()
}

unsafe fn get_property_strings(
    info_set: HDEVINFO,
    info: &SP_DEVINFO_DATA,
    property: u32,
) -> Vec<String> {
    let Some((reg_type, bytes)) = get_property_raw(info_set, info, property) else {
        return Vec::new();
    };

    match reg_type {
        REG_MULTI_SZ => utf16_multi_to_strings(&bytes),
        REG_SZ | REG_EXPAND_SZ => {
            let s = utf16z_to_string(&bytes_to_u16s(&bytes));
            if s.is_empty() {
                Vec::new()
            } else {
                vec![s]
            }
        }
        _ => {
            let s = utf16z_to_string(&bytes_to_u16s(&bytes));
            if s.is_empty() {
                Vec::new()
            } else {
                vec![s]
            }
        }
    }
}

unsafe fn get_property_raw(
    info_set: HDEVINFO,
    info: &SP_DEVINFO_DATA,
    property: u32,
) -> Option<(u32, Vec<u8>)> {
    let mut reg_type = 0;
    let mut required = 0;
    let ok = SetupDiGetDeviceRegistryPropertyW(
        info_set,
        info,
        property,
        &mut reg_type,
        null_mut(),
        0,
        &mut required,
    );
    if ok == 0 && GetLastError() != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }
    if required == 0 {
        return None;
    }

    let mut buffer = vec![0u8; required as usize];
    if SetupDiGetDeviceRegistryPropertyW(
        info_set,
        info,
        property,
        &mut reg_type,
        buffer.as_mut_ptr(),
        buffer.len() as u32,
        &mut required,
    ) == 0
    {
        return None;
    }
    buffer.truncate(required as usize);

    Some((reg_type, buffer))
}

fn parse_vid_pid(id: &str) -> Option<(u16, u16)> {
    let upper = id.to_ascii_uppercase();
    let vid = parse_hex_after(&upper, "VID_")?;
    let pid = parse_hex_after(&upper, "PID_")?;
    Some((vid, pid))
}

fn parse_hex_after(haystack: &str, marker: &str) -> Option<u16> {
    let start = haystack.find(marker)? + marker.len();
    let hex = haystack.get(start..start + 4)?;
    hex.chars().all(|c| c.is_ascii_hexdigit()).then_some(())?;
    u16::from_str_radix(hex, 16).ok()
}

fn normalize_driver_name(driver: &str) -> String {
    let trimmed = driver.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "winusb" => "WinUSB".to_string(),
        "hidusb" => "HidUsb".to_string(),
        "bthusb" => "BTHUSB".to_string(),
        "usbccgp" => "usbccgp".to_string(),
        "usbser" => "usbser".to_string(),
        _ => trimmed.to_string(),
    }
}

fn is_composite_device(hardware_ids: &[String], compatible_ids: &[String], driver: &str) -> bool {
    hardware_ids
        .iter()
        .any(|id| id.to_ascii_uppercase().contains("&MI_"))
        || compatible_ids
            .iter()
            .any(|id| id.eq_ignore_ascii_case("USB\\COMPOSITE"))
        || driver.eq_ignore_ascii_case("usbccgp")
}

fn is_hub_or_controller(description: &str, driver: &str, class: &str) -> bool {
    let driver = driver.to_ascii_lowercase();
    if driver == "usbhub" || driver == "usbhub3" {
        return true;
    }

    let description = description.to_ascii_lowercase();
    if description.contains("root hub") || description == "generic usb hub" {
        return true;
    }

    class.eq_ignore_ascii_case("usb") && description.contains("host controller")
}

fn first_non_empty(values: &[String]) -> Option<String> {
    values
        .iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn bytes_to_u16s(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect()
}

fn utf16z_to_string(chars: &[u16]) -> String {
    let len = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
    String::from_utf16_lossy(&chars[..len]).trim().to_string()
}

fn utf16_multi_to_strings(bytes: &[u8]) -> Vec<String> {
    let chars = bytes_to_u16s(bytes);
    let mut values = Vec::new();
    let mut start = 0;

    for (i, &ch) in chars.iter().enumerate() {
        if ch != 0 {
            continue;
        }
        if i == start {
            break;
        }
        let value = String::from_utf16_lossy(&chars[start..i])
            .trim()
            .to_string();
        if !value.is_empty() {
            values.push(value);
        }
        start = i + 1;
    }

    values
}

pub fn find_device(vid: u16, pid: u16) -> Result<Option<UsbDevice>, String> {
    Ok(list_devices(true)?
        .into_iter()
        .find(|device| device.vid == vid && device.pid == pid))
}

pub fn write_winusb_package(vid: u16, pid: u16, work_dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(work_dir)
        .map_err(|e| format!("could not create WinUSB package dir {:?}: {}", work_dir, e))?;

    let device = find_device(vid, pid)?;
    let description = device
        .as_ref()
        .map(|d| d.description.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Aokie Bluetooth Dongle");

    let package = winusb::WinusbPackage::new(vid, pid, description);
    let inf_path = work_dir.join(winusb::INF_NAME);
    std::fs::write(&inf_path, package.render_inf())
        .map_err(|e| format!("could not write WinUSB INF {:?}: {}", inf_path, e))?;

    Ok(inf_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vid_pid_from_usb_ids() {
        assert_eq!(
            parse_vid_pid("USB\\VID_0A5C&PID_21EC\\ABC"),
            Some((0x0a5c, 0x21ec))
        );
        assert_eq!(
            parse_vid_pid("usb\\vid_0bda&pid_8771&mi_00"),
            Some((0x0bda, 0x8771))
        );
    }

    #[test]
    fn rejects_invalid_vid_pid_ids() {
        assert_eq!(parse_vid_pid("USB\\VID_0A5C\\ABC"), None);
        assert_eq!(parse_vid_pid("USB\\VID_ZZZZ&PID_21EC"), None);
    }

    #[test]
    fn normalizes_driver_names_used_by_pairing_ui() {
        assert_eq!(normalize_driver_name(" winusb "), "WinUSB");
        assert_eq!(normalize_driver_name("HIDUSB"), "HidUsb");
        assert_eq!(normalize_driver_name("usbccgp"), "usbccgp");
        assert_eq!(normalize_driver_name(""), "");
    }

    #[test]
    fn detects_composite_usb_devices() {
        assert!(is_composite_device(
            &["USB\\VID_1234&PID_5678&MI_00".to_string()],
            &[],
            "WinUSB",
        ));
        assert!(is_composite_device(
            &[],
            &["USB\\COMPOSITE".to_string()],
            "",
        ));
        assert!(is_composite_device(&[], &[], "usbccgp"));
        assert!(!is_composite_device(&[], &[], "WinUSB"));
    }
}
