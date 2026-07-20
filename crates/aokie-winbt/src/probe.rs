//! Phase-0 capability probe (docs/NATIVE_BLUETOOTH_TRANSPORT_PLAN.md §6).
//!
//! Verifies, on the current machine, every Windows capability the native
//! backend depends on — WITHOUT changing system state (no pairing, no
//! registration, no streaming):
//!   1. Bluetooth adapters visible to Windows.
//!   2. Paired classic-Bluetooth devices + their RFCOMM services (MAP/PBAP).
//!   3. THE GO/NO-GO: `PhoneCallStore::RequestLineWatcher()` from this
//!      unpackaged process (restricted capabilities).
//!   4. `PhoneLineTransportDevice::GetDefault()`.
//!   5. WASAPI Hands-Free render/capture endpoints + their mix formats.

use windows::core::Error;

fn hr(e: &Error) -> String {
    format!("0x{:08X} {}", e.code().0, e.message())
}

pub fn run() {
    println!("=== aokie winbt probe (phase 0) ===");
    unsafe {
        use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_TYPE};
        match RoInitialize(RO_INIT_TYPE(1)) {
            Ok(()) => println!("[ok] RoInitialize(MULTITHREADED)"),
            Err(e) if e.code().0 == 1 => println!("[ok] RoInitialize: already initialized"),
            Err(e) => println!("[FAIL] RoInitialize: {}", hr(&e)),
        }
    }

    probe_adapters();
    probe_paired_devices();
    probe_calls_api();
    probe_hf_audio();
    println!("=== probe done ===");
}

fn probe_adapters() {
    println!("\n--- Bluetooth adapters ---");
    use windows::Devices::Bluetooth::BluetoothAdapter;
    match BluetoothAdapter::GetDefaultAsync() {
        Ok(op) => match op.get() {
            Ok(adapter) => {
                let addr = adapter.BluetoothAddress().unwrap_or(0);
                let radio = adapter.IsLowEnergySupported().unwrap_or(false);
                println!(
                    "[ok] default adapter: address {:012X}, BLE supported: {}",
                    addr, radio
                );
                match adapter.IsClassicSupported() {
                    Ok(c) => println!("[ok] classic (BR/EDR) supported: {c}"),
                    Err(e) => println!("[warn] IsClassicSupported: {}", hr(&e)),
                }
                match adapter.AreClassicSecureConnectionsSupported() {
                    Ok(c) => println!("[info] classic secure connections: {c}"),
                    Err(_) => {}
                }
            }
            Err(e) => println!(
                "[FAIL] no default adapter (Bluetooth off / no radio?): {}",
                hr(&e)
            ),
        },
        Err(e) => println!("[FAIL] BluetoothAdapter::GetDefaultAsync: {}", hr(&e)),
    }
    // Every adapter (a box can have a built-in radio + a dongle):
    use windows::Devices::Enumeration::DeviceInformation;
    let sel = BluetoothAdapter::GetDeviceSelector().unwrap_or_default();
    match DeviceInformation::FindAllAsyncAqsFilter(&sel) {
        Ok(op) => match op.get() {
            Ok(devs) => {
                let n = devs.Size().unwrap_or(0);
                println!("[ok] {n} Bluetooth adapter device(s) enumerated");
                for d in devs {
                    println!(
                        "     - {} | {} | enabled={}",
                        d.Name().unwrap_or_default(),
                        d.Id().unwrap_or_default(),
                        d.IsEnabled().unwrap_or(false)
                    );
                }
            }
            Err(e) => println!("[warn] adapter enumeration: {}", hr(&e)),
        },
        Err(e) => println!("[warn] adapter FindAllAsync: {}", hr(&e)),
    }
}

fn probe_paired_devices() {
    println!("\n--- Paired classic Bluetooth devices + RFCOMM services ---");
    use windows::Devices::Bluetooth::BluetoothDevice;
    use windows::Devices::Enumeration::DeviceInformation;
    let sel = BluetoothDevice::GetDeviceSelectorFromPairingState(true).unwrap_or_default();
    let devs = match DeviceInformation::FindAllAsyncAqsFilter(&sel) {
        Ok(op) => match op.get() {
            Ok(d) => d,
            Err(e) => {
                println!("[warn] paired-device enumeration: {}", hr(&e));
                return;
            }
        },
        Err(e) => {
            println!("[warn] FindAllAsync: {}", hr(&e));
            return;
        }
    };
    let n = devs.Size().unwrap_or(0);
    println!("[ok] {n} paired Bluetooth device(s)");
    for d in devs {
        let id = d.Id().unwrap_or_default();
        let name = d.Name().unwrap_or_default();
        println!("     - {name} | {id}");
        if let Ok(op) = BluetoothDevice::FromIdAsync(&id) {
            if let Ok(dev) = op.get() {
                let addr = dev.BluetoothAddress().unwrap_or(0);
                println!(
                    "       address {:012X} connected={:?}",
                    addr,
                    dev.ConnectionStatus().map(|s| s.0).unwrap_or(-1)
                );
                if let Ok(sop) = dev.GetRfcommServicesAsync() {
                    if let Ok(services) = sop.get() {
                        if let Ok(list) = services.Services() {
                            let mut found = Vec::new();
                            for s in list {
                                if let Ok(sid) = s.ServiceId() {
                                    if let Ok(uuid) = sid.Uuid() {
                                        found.push(format!("{:?}", uuid));
                                    }
                                }
                            }
                            println!(
                                "       rfcomm services ({}): {}",
                                found.len(),
                                found.join(", ")
                            );
                        }
                    }
                }
                // MAP MAS specifically (0x1132):
                use windows::Devices::Bluetooth::Rfcomm::RfcommServiceId;
                if let Ok(sid) = RfcommServiceId::FromShortId(crate::MAP_MAS_SHORT_UUID as u32) {
                    match dev.GetRfcommServicesForIdAsync(&sid) {
                        Ok(op) => match op.get() {
                            Ok(res) => {
                                let cnt =
                                    res.Services().map(|l| l.Size().unwrap_or(0)).unwrap_or(0);
                                let err =
                                    res.Error().map(|e| format!("{:?}", e)).unwrap_or_default();
                                println!(
                                    "       MAP MAS 0x1132: status={} service(s={})",
                                    err, cnt
                                );
                            }
                            Err(e) => println!("       MAP MAS 0x1132: FAIL {}", hr(&e)),
                        },
                        Err(e) => println!("       MAP MAS 0x1132: call failed {}", hr(&e)),
                    }
                }
            }
        }
    }
    if n == 0 {
        println!("     (pair the phone in Windows Settings to test MAP/PBAP reachability)");
    }
}

fn probe_calls_api() {
    println!("\n--- Calls API (GO/NO-GO) ---");
    use windows::ApplicationModel::Calls::{PhoneCallManager, PhoneLineTransportDevice};
    // 1. The store → line watcher (restricted capability check).
    match PhoneCallManager::RequestStoreAsync() {
        Ok(op) => match op.get() {
            Ok(store) => {
                println!("[ok] PhoneCallStore acquired");
                match store.RequestLineWatcher() {
                    Ok(watcher) => {
                        println!("[ok] RequestLineWatcher() SUCCEEDED — watcher created");
                        // Start to see if an access error arrives asynchronously.
                        match watcher.Start() {
                            Ok(()) => println!("[ok] watcher.Start() ok; status={:?}", watcher.Status().map(|s| format!("{:?}", s))),
                            Err(e) => println!("[FAIL] watcher.Start(): {}", hr(&e)),
                        }
                        let _ = watcher.Stop();
                    }
                    Err(e) => println!(
                        "[FAIL — GO/NO-GO] RequestLineWatcher() refused: {} (restricted capability / package identity needed?)",
                        hr(&e)
                    ),
                }
            }
            Err(e) => println!("[FAIL] PhoneCallStore.get(): {}", hr(&e)),
        },
        Err(e) => println!("[FAIL] PhoneCallManager::RequestStoreAsync: {}", hr(&e)),
    }
    // 2. Transport registration status (instance APIs only exist per device id
    //    in the windows-rs projection, so the watcher above is the go/no-go).
    let _ = std::marker::PhantomData::<PhoneLineTransportDevice>;
    println!(
        "[info] PhoneLineTransportDevice: registration happens at backend start (RegisterApp)"
    );
}

fn probe_hf_audio() {
    println!("\n--- WASAPI Hands-Free endpoints ---");
    use windows::Win32::Media::Audio::{
        eCapture, eRender, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
        DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    unsafe {
        let r = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
        // RPC_E_CHANGED_MODE (0x80010106, already initialized) is fine.
        if r.is_err() && r.0 != -2147417850i32 {
            println!("[warn] CoInitializeEx: 0x{:08X}", r.0);
        }
        let enumerator: IMMDeviceEnumerator =
            match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                Ok(e) => e,
                Err(e) => {
                    println!("[FAIL] MMDeviceEnumerator: {}", hr(&e));
                    return;
                }
            };
        for (flow, label) in [(eRender, "render"), (eCapture, "capture")] {
            let coll = match enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) {
                Ok(c) => c,
                Err(e) => {
                    println!("[warn] EnumAudioEndpoints({label}): {}", hr(&e));
                    continue;
                }
            };
            let count = coll.GetCount().unwrap_or(0);
            let mut hf = 0u32;
            for i in 0..count {
                let dev = match coll.Item(i) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                let id = dev
                    .GetId()
                    .map(|p| p.to_string().unwrap_or_default())
                    .unwrap_or_default();
                let name = read_prop(
                    &dev,
                    &windows::Win32::Devices::FunctionDiscovery::PKEY_DeviceInterface_FriendlyName,
                )
                .or_else(|| {
                    read_prop(
                        &dev,
                        &windows::Win32::Devices::FunctionDiscovery::PKEY_Device_DeviceDesc,
                    )
                })
                .unwrap_or_default();
                let lower = format!("{name} {id}").to_lowercase();
                let is_hf = lower.contains("hands-free")
                    || lower.contains("bthhf")
                    || lower.contains("headset");
                if is_hf {
                    hf += 1;
                }
                let mark = if is_hf { " <-- HANDS-FREE" } else { "" };
                println!("     [{label}] {name} | {id}{mark}");
                if is_hf {
                    // Mix format reveals negotiated rate (16 kHz = WBS/mSBC).
                    if let Ok(client) = dev.Activate::<IAudioClient>(CLSCTX_ALL, None) {
                        if let Ok(fmt) = client.GetMixFormat() {
                            // WAVEFORMATEX is packed — copy fields before use.
                            let (hz, ch, tag, bits) = {
                                let f = &*fmt;
                                (
                                    f.nSamplesPerSec,
                                    f.nChannels,
                                    f.wFormatTag,
                                    f.wBitsPerSample,
                                )
                            };
                            println!(
                                "         mix format: {} Hz, {} ch, tag {} bits {}",
                                hz, ch, tag, bits
                            );
                        }
                    }
                }
            }
            if hf == 0 {
                println!("     [{label}] no Hands-Free endpoint (pair an HFP phone to see one)");
            }
        }
        let _ = enumerator;
    }
}

#[cfg(target_os = "windows")]
unsafe fn read_prop(
    dev: &windows::Win32::Media::Audio::IMMDevice,
    key: &windows::Win32::Foundation::PROPERTYKEY,
) -> Option<String> {
    use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
    use windows::Win32::System::Com::STGM_READ;
    let store = dev.OpenPropertyStore(STGM_READ).ok()?;
    let pv = store.GetValue(key).ok()?;
    let s = PropVariantToStringAlloc(std::ptr::from_ref(&pv)).ok()?;
    Some(s.to_string().unwrap_or_default())
}
