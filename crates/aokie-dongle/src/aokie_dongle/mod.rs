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
    /// The unique device-instance id, e.g.
    /// `USB\VID_0A5C&PID_21EC\00198600226C`. Distinct per physical
    /// plug-in (unlike `hardware_id`, which is just the VID/PID prefix
    /// shared by every unit of a model). AOK-DRIVER-001 signs this into
    /// the elevated install job so the helper can confirm it is binding
    /// the EXACT device the operator selected, not a same-model unit
    /// swapped in after approval.
    pub instance_id: String,
    /// `SPDRP_CLASS` (e.g. "USBDevice", "Bluetooth", "HIDClass"). Fed to
    /// the install-policy deny-list.
    pub class: String,
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
            instance_id,
            class,
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

/// AOK-DRIVER-001: whether the operator has opted in to rebinding a
/// dongle that isn't in the catalog. Release builds demand the verbose
/// [`UNKNOWN_DONGLE_OPT_IN`] sentinel so it can't be flipped by a stray
/// `=1`; debug builds also accept `1` for local bring-up of a new
/// chipset. Read fresh on every call (env can change between installs).
pub fn allow_unknown_dongle() -> bool {
    use aokie_core::dongle_catalog::UNKNOWN_DONGLE_OPT_IN;
    match std::env::var("AOKIE_INSTALL_UNKNOWN_DONGLE") {
        Ok(v) if v == UNKNOWN_DONGLE_OPT_IN => true,
        #[cfg(debug_assertions)]
        Ok(v) if v == "1" => true,
        _ => false,
    }
}

/// AOK-DRIVER-001 install gate, shared by the unelevated dispatcher and
/// the elevated helper. Enumerates the LIVE device set, finds the
/// (vid, pid) target, and runs it through the pure
/// [`evaluate_install_target`](aokie_core::dongle_catalog::evaluate_install_target)
/// policy. On success returns the matched [`UsbDevice`] (whose
/// `instance_id` the caller signs into / checks against the job); on
/// failure returns the policy's actionable message. A device that isn't
/// enumerated at all is `NotPresent` — Aokie never stages a driver
/// against an absent device.
pub fn evaluate_present_target(
    vid: u16,
    pid: u16,
    allow_unknown: bool,
) -> Result<UsbDevice, String> {
    use aokie_core::dongle_catalog::{evaluate_install_target, DeviceFacts, InstallRejection};

    let device = find_device(vid, pid)?;
    // `list_devices` already filters out hubs / host controllers, so a
    // found device is never one; an absent target surfaces as NotPresent.
    let facts = match &device {
        Some(d) => DeviceFacts {
            vid: d.vid,
            pid: d.pid,
            present: true,
            is_composite: d.is_composite,
            is_hub_or_controller: false,
            class: &d.class,
        },
        None => DeviceFacts {
            vid,
            pid,
            present: false,
            is_composite: false,
            is_hub_or_controller: false,
            class: "",
        },
    };

    match evaluate_install_target(&facts, allow_unknown) {
        Ok(approval) => {
            let d = device.expect("present target implies Some(device)");
            if approval.unknown_opt_in {
                aokie_core::redact::audit(
                    "winusb_install_unknown_dongle",
                    format!(
                        "vid={:04x} pid={:04x} instance={} — admitted via opt-in",
                        d.vid, d.pid, d.instance_id
                    ),
                );
            }
            Ok(d)
        }
        Err(rejection @ InstallRejection::NotPresent { .. }) => Err(rejection.message()),
        Err(rejection) => {
            // A refusal on a device that IS plugged in is worth an audit
            // line — it's the tamper / mistaken-target signal.
            aokie_core::redact::audit(
                "winusb_install_refused",
                format!("vid={:04x} pid={:04x}: {:?}", vid, pid, rejection),
            );
            Err(rejection.message())
        }
    }
}

/// SHA-256 the file at `path`, lowercase hex. Streams in 64 KiB chunks.
/// Shared by the unelevated dispatcher (to sign the rendered INF into
/// the job) and the elevated helper (to verify the INF bytes on disk
/// match what was approved before staging the driver).
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| format!("open {:?}: {}", path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read {:?}: {}", path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{:02x}", byte);
    }
    Ok(hex)
}

pub fn write_winusb_package(vid: u16, pid: u16, work_dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(work_dir)
        .map_err(|e| format!("could not create WinUSB package dir {:?}: {}", work_dir, e))?;

    // DIST-01: production uses the unchanged static INF/CAT pair returned by
    // Microsoft signing. It lives beside the release in `driver-package/`;
    // an explicit directory override is useful only for packaging tests. The
    // elevated helper independently pins both digests, so this unelevated copy
    // is transport rather than a trust boundary.
    let package_dir = std::env::var_os("AOKIE_WINUSB_PACKAGE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|parent| parent.join("driver-package")))
        });
    if let Some(package_dir) = package_dir {
        let source_inf = package_dir.join(winusb::INF_NAME);
        let source_cat = package_dir.join(winusb::CAT_NAME);
        if source_inf.is_file() && source_cat.is_file() {
            let inf_text = std::fs::read_to_string(&source_inf)
                .map_err(|e| format!("could not read static WinUSB INF {:?}: {}", source_inf, e))?;
            let hardware_id = winusb::hardware_id(vid, pid);
            if !inf_text.to_ascii_uppercase().contains(&hardware_id) {
                return Err(format!(
                    "the signed WinUSB INF does not cover {}; obtain a new Microsoft-signed package",
                    hardware_id
                ));
            }
            let inf_path = work_dir.join(winusb::INF_NAME);
            let cat_path = work_dir.join(winusb::CAT_NAME);
            std::fs::copy(&source_inf, &inf_path)
                .map_err(|e| format!("could not stage static INF {:?}: {}", source_inf, e))?;
            std::fs::copy(&source_cat, &cat_path)
                .map_err(|e| format!("could not stage static catalog {:?}: {}", source_cat, e))?;
            return Ok(inf_path);
        }
    }

    if !cfg!(debug_assertions) && !cfg!(feature = "managed-beta-driver") {
        return Err(format!(
            "the signed WinUSB package is missing; {} and {} must be present in driver-package/ beside the plugin",
            winusb::INF_NAME,
            winusb::CAT_NAME
        ));
    }

    // Development / managed-beta fallback only. The elevated helper compares
    // these exact bytes with its own trusted renderer before generating a
    // local catalog, so an unelevated process cannot smuggle arbitrary INF
    // directives into the privileged install. The runtime opt-in is checked
    // separately by the dispatcher and helper.
    let device = find_device(vid, pid)?;
    let description = device
        .as_ref()
        .map(|d| d.description.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Aokie Bluetooth Dongle");

    let package = winusb::WinusbPackage::new(vid, pid, description);
    let inf_path = work_dir.join(winusb::INF_NAME);
    // A reused work directory must not accidentally pair the freshly rendered
    // managed-beta INF with a stale catalog from an earlier production stage.
    let stale_cat = work_dir.join(winusb::CAT_NAME);
    if stale_cat.exists() {
        std::fs::remove_file(&stale_cat).map_err(|e| {
            format!(
                "could not remove stale WinUSB catalog {:?}: {}",
                stale_cat, e
            )
        })?;
    }
    std::fs::write(&inf_path, package.render_inf())
        .map_err(|e| format!("could not write WinUSB INF {:?}: {}", inf_path, e))?;

    Ok(inf_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_release_inf_covers_every_supported_hardware_id() {
        let inf = include_str!("../../../../drivers/winusb/aokie_winusb_bluetooth.inf")
            .to_ascii_uppercase();
        for dongle in aokie_core::dongle_catalog::DEFAULT_CATALOG {
            let hardware_id = winusb::hardware_id(dongle.vid, dongle.pid);
            assert!(
                inf.contains(&hardware_id),
                "static release INF omits {hardware_id}"
            );
        }
    }

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
