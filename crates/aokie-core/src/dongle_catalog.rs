//! Cross-platform catalog of known Aokie Bluetooth dongles.
//!
//! Single source of truth for VID/PID pairs the app recognises as
//! "an Aokie dongle". Both the Windows WinUSB-bind UX and the Linux
//! udev-rule renderer consume this list, and the frontend pulls the
//! same data through a Tauri command (`list_known_dongles`, a thin
//! wrapper in `aokie-desktop/src-tauri/src/aokie_dongle_catalog.rs`)
//! so the setup orchestrator's "looks like a Bluetooth dongle"
//! heuristic stays in sync without a duplicated TS-side hardcoded
//! list. The FormLogic Desktop plugin (`crates/aokie-plugin`) serves
//! the same catalog through its `dongle.list` connector command.
//!
//! The catalog is curated rather than open: the list intentionally
//! sticks to chipsets we've smoke-tested (Broadcom BCM2070x, Realtek
//! RTL8761/RTL8821CE). Adding a new chipset means appending here and
//! verifying the HCI transport works end-to-end on it.

use serde::Serialize;

/// Smoke-test tier for each catalog entry. Surfaced to the frontend
/// (R11-#10) so the Pairing UI can put a badge next to a dongle row
/// instead of letting the operator find out post-purchase that their
/// chipset is "kind of works, sometimes" rather than the certified
/// path. Anything not in `DEFAULT_CATALOG` is implicitly Unsupported
/// — the install boundary already refuses unknown VID/PID pairs
/// unless `AOKIE_INSTALL_UNKNOWN_DONGLE=YES_I_REBIND_AT_MY_OWN_RISK`
/// opts in (R13/15: the simple `=1` form is now debug-only — release
/// builds require the deliberately-ugly sentinel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DongleCompatTier {
    /// End-to-end tested, known good on the dev rig and reproducible
    /// across at least one external box. Broadcom BCM2070x sits here.
    Certified,
    /// Boots and pairs but has known quirks (codec / SCO routing /
    /// firmware loader) that may surface on some chassis. Realtek and
    /// CSR families.
    Beta,
}

/// One VID/PID pair the dongle recognizer matches against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DongleId {
    pub vid: u16,
    pub pid: u16,
    pub tier: DongleCompatTier,
}

/// Default catalog. Real-world Linux Bluetooth dongles are dominated
/// by Realtek (rtlbtsd) and Broadcom (BCM2070x); the list is open by
/// design so users with other hardware can extend it via PR.
pub const DEFAULT_CATALOG: &[DongleId] = &[
    // Broadcom BCM20702 — common 2.4 GHz combo dongle.
    DongleId {
        vid: 0x0a5c,
        pid: 0x21e8,
        tier: DongleCompatTier::Certified,
    },
    DongleId {
        vid: 0x0a5c,
        pid: 0x21ec,
        tier: DongleCompatTier::Certified,
    },
    // Realtek RTL8761 — the chipset most ASUS / TP-Link dongles use.
    DongleId {
        vid: 0x0bda,
        pid: 0x8771,
        tier: DongleCompatTier::Beta,
    },
    // Realtek RTL8821CE — newer combo cards seen in some HP / Lenovo
    // laptops also expose a USB Bluetooth interface with these IDs.
    DongleId {
        vid: 0x0bda,
        pid: 0xc822,
        tier: DongleCompatTier::Beta,
    },
    // Cambridge Silicon Radio CSR8510 A10 — ubiquitous BT 4.0 dongle,
    // no vendor firmware load required.
    DongleId {
        vid: 0x0a12,
        pid: 0x0001,
        tier: DongleCompatTier::Beta,
    },
];

/// Returns the catalog as plain owned data so callers (the legacy
/// Tauri command wrapper, the plugin's `dongle.list` handler) can
/// hand it to a frontend without holding a Rust reference. Pure;
/// safe to call on every orchestrator mount.
pub fn list_known_dongles() -> Vec<DongleId> {
    DEFAULT_CATALOG.to_vec()
}

/// Catalog lookup: the tier for a (vid, pid) pair, or `None` if the
/// pair isn't a recognised Aokie dongle. The install boundary treats
/// `None` as "unknown hardware" and refuses unless the operator has
/// set the deliberately-ugly opt-in sentinel (AOK-DRIVER-001).
pub fn dongle_tier(vid: u16, pid: u16) -> Option<DongleCompatTier> {
    DEFAULT_CATALOG
        .iter()
        .find(|d| d.vid == vid && d.pid == pid)
        .map(|d| d.tier)
}

/// The USB device facts the install policy reasons about. Gathered by
/// the platform enumerator (`aokie-dongle` on Windows) and fed to
/// [`evaluate_install_target`] so the IDENTICAL rules run at BOTH the
/// unelevated dispatch boundary and inside the elevated helper
/// (AOK-DRIVER-001 defence-in-depth: a tampered job that swaps in a
/// keyboard / internal combo / phantom VID-PID is re-judged against the
/// live device before the driver is staged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceFacts<'a> {
    pub vid: u16,
    pub pid: u16,
    /// The device is physically enumerated right now. An install target
    /// that isn't present can't be identity-checked, and staging an INF
    /// "for next plug-in" against an absent device is exactly how an
    /// unintended device gets rebound later.
    pub present: bool,
    /// A composite device (exposes `&MI_` child interfaces / binds
    /// usbccgp). These are almost always internal combo radios where the
    /// Bluetooth function shares silicon with Wi-Fi / the laptop's real
    /// BT — rebinding the parent to WinUSB disables all of it.
    pub is_composite: bool,
    /// A USB hub or host controller — never a valid target.
    pub is_hub_or_controller: bool,
    /// `SPDRP_CLASS`, e.g. "USBDevice" (WinUSB-bound), "Bluetooth"
    /// (BTHUSB-bound), "HIDClass", "USB". Used for a deny-list so an
    /// obviously-wrong device (keyboard, disk, printer, net) can't be
    /// rebound even through the unknown-dongle opt-in.
    pub class: &'a str,
}

/// Why an install target was refused. Carries enough for an actionable
/// operator message and for tests to assert on the exact reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallRejection {
    /// (vid, pid) isn't in the catalog and the unknown-dongle opt-in
    /// wasn't set.
    UnknownDongle { vid: u16, pid: u16 },
    /// The device isn't currently enumerated.
    NotPresent { vid: u16, pid: u16 },
    /// A USB hub / host controller.
    HubOrController,
    /// A composite / internal combo device — refusing protects the
    /// machine's built-in Bluetooth / Wi-Fi.
    Composite,
    /// The device's class is on the deny-list (keyboard, storage, …).
    DisallowedClass { class: String },
}

impl InstallRejection {
    /// Operator-facing one-liner.
    pub fn message(&self) -> String {
        match self {
            InstallRejection::UnknownDongle { vid, pid } => format!(
                "USB {:04x}:{:04x} is not a recognised Aokie Bluetooth dongle — refusing to \
                 rebind it. If you are certain this is an external BT dongle, set \
                 AOKIE_INSTALL_UNKNOWN_DONGLE={} and retry.",
                vid, pid, UNKNOWN_DONGLE_OPT_IN
            ),
            InstallRejection::NotPresent { vid, pid } => format!(
                "no {:04x}:{:04x} device is plugged in — plug the dongle in before installing its \
                 driver (Aokie will not stage a driver against an absent device).",
                vid, pid
            ),
            InstallRejection::HubOrController => {
                "that device is a USB hub or host controller, not a Bluetooth dongle — refusing."
                    .to_string()
            }
            InstallRejection::Composite => {
                "that device is a composite / internal combo adapter (its Bluetooth shares hardware \
                 with other functions). Rebinding it to WinUSB would disable your built-in \
                 Bluetooth/Wi-Fi, so Aokie refuses. Use a separate external USB dongle."
                    .to_string()
            }
            InstallRejection::DisallowedClass { class } => format!(
                "that device reports class {:?}, which is never a Bluetooth dongle (e.g. keyboard, \
                 storage, printer, network) — refusing to rebind it.",
                class
            ),
        }
    }
}

/// The value `AOKIE_INSTALL_UNKNOWN_DONGLE` must hold to bypass the
/// catalog check. Deliberately verbose so it can't be set by accident
/// or by a simple `=1` a script author copies without understanding.
pub const UNKNOWN_DONGLE_OPT_IN: &str = "YES_I_REBIND_AT_MY_OWN_RISK";

/// Device classes that are NEVER a Bluetooth dongle. Matched
/// case-insensitively against `SPDRP_CLASS`. This is a deny-list, not an
/// allow-list, because a legitimate dongle legitimately appears under
/// several classes across its lifecycle ("Bluetooth" unbound,
/// "USBDevice" once WinUSB-bound, sometimes bare "USB"); an allow-list
/// would reject valid targets, whereas a deny-list only has to name the
/// obviously-wrong ones the catalog opt-in might otherwise wave through.
const DENIED_DEVICE_CLASSES: &[&str] = &[
    "hidclass",
    "keyboard",
    "mouse",
    "diskdrive",
    "usbstor",
    "cdrom",
    "printer",
    "net",
    "media",
    "image",
    "smartcardreader",
    "monitor",
    "camera",
    "wpd",
];

/// True when `class` is on the deny-list. Empty / unknown classes are
/// allowed (a fresh dongle may report no class), so this only fires on
/// an affirmatively-wrong class.
pub fn class_is_denied(class: &str) -> bool {
    let c = class.trim().to_ascii_lowercase();
    !c.is_empty() && DENIED_DEVICE_CLASSES.iter().any(|d| c == *d)
}

/// Approval detail returned when an install target passes policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallApproval {
    /// The catalog tier, or `None` when admitted only via the
    /// unknown-dongle opt-in.
    pub tier: Option<DongleCompatTier>,
    /// True when this target is NOT in the catalog and was admitted
    /// solely because the opt-in sentinel was set.
    pub unknown_opt_in: bool,
}

/// The single source of truth for "may Aokie stage its WinUSB driver
/// against this device?" — pure, so the unelevated dispatcher and the
/// elevated helper reach the same verdict from the same facts
/// (AOK-DRIVER-001). Order matters: physical-safety refusals (absent /
/// hub / composite / bad class) come before the catalog check so the
/// message is the most specific true reason.
pub fn evaluate_install_target(
    facts: &DeviceFacts<'_>,
    allow_unknown: bool,
) -> Result<InstallApproval, InstallRejection> {
    if !facts.present {
        return Err(InstallRejection::NotPresent {
            vid: facts.vid,
            pid: facts.pid,
        });
    }
    if facts.is_hub_or_controller {
        return Err(InstallRejection::HubOrController);
    }
    if facts.is_composite {
        return Err(InstallRejection::Composite);
    }
    if class_is_denied(facts.class) {
        return Err(InstallRejection::DisallowedClass {
            class: facts.class.trim().to_string(),
        });
    }
    match dongle_tier(facts.vid, facts.pid) {
        Some(tier) => Ok(InstallApproval {
            tier: Some(tier),
            unknown_opt_in: false,
        }),
        None if allow_unknown => Ok(InstallApproval {
            tier: None,
            unknown_opt_in: true,
        }),
        None => Err(InstallRejection::UnknownDongle {
            vid: facts.vid,
            pid: facts.pid,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_helper_matches_constant_length() {
        assert_eq!(list_known_dongles().len(), DEFAULT_CATALOG.len());
    }

    #[test]
    fn catalog_helper_round_trips_first_entry() {
        let entries = list_known_dongles();
        assert_eq!(entries[0], DEFAULT_CATALOG[0]);
    }

    /// The tier serialisation is part of the frontend contract — the
    /// Pairing UI matches on the lowercase strings.
    #[test]
    fn tier_serialises_lowercase() {
        let json = serde_json::to_value(DEFAULT_CATALOG[0]).unwrap();
        assert_eq!(json["tier"], "certified");
        let json = serde_json::to_value(DEFAULT_CATALOG[2]).unwrap();
        assert_eq!(json["tier"], "beta");
    }

    // ---- AOK-DRIVER-001 install-policy decision table ----

    /// A catalog dongle, present, non-composite, benign class.
    fn good_facts() -> DeviceFacts<'static> {
        DeviceFacts {
            vid: 0x0a5c,
            pid: 0x21ec,
            present: true,
            is_composite: false,
            is_hub_or_controller: false,
            class: "USBDevice",
        }
    }

    #[test]
    fn dongle_tier_maps_catalog_and_rejects_strangers() {
        assert_eq!(
            dongle_tier(0x0a5c, 0x21ec),
            Some(DongleCompatTier::Certified)
        );
        assert_eq!(dongle_tier(0x0bda, 0x8771), Some(DongleCompatTier::Beta));
        assert_eq!(dongle_tier(0x1234, 0x5678), None);
    }

    #[test]
    fn approves_known_present_dongle() {
        let approval = evaluate_install_target(&good_facts(), false).unwrap();
        assert_eq!(approval.tier, Some(DongleCompatTier::Certified));
        assert!(!approval.unknown_opt_in);
    }

    #[test]
    fn rejects_absent_device_before_anything_else() {
        let facts = DeviceFacts {
            present: false,
            ..good_facts()
        };
        assert_eq!(
            evaluate_install_target(&facts, true).unwrap_err(),
            InstallRejection::NotPresent {
                vid: 0x0a5c,
                pid: 0x21ec
            }
        );
    }

    #[test]
    fn rejects_hub_and_composite_even_with_opt_in() {
        let hub = DeviceFacts {
            is_hub_or_controller: true,
            ..good_facts()
        };
        assert_eq!(
            evaluate_install_target(&hub, true).unwrap_err(),
            InstallRejection::HubOrController
        );
        let combo = DeviceFacts {
            is_composite: true,
            ..good_facts()
        };
        assert_eq!(
            evaluate_install_target(&combo, true).unwrap_err(),
            InstallRejection::Composite
        );
    }

    #[test]
    fn rejects_denied_classes_even_with_opt_in() {
        // A keyboard whose VID/PID somehow matched the opt-in path must
        // still fail on class.
        let kbd = DeviceFacts {
            vid: 0x1234,
            pid: 0x5678,
            class: "HIDClass",
            ..good_facts()
        };
        match evaluate_install_target(&kbd, true).unwrap_err() {
            InstallRejection::DisallowedClass { class } => assert_eq!(class, "HIDClass"),
            other => panic!("expected DisallowedClass, got {:?}", other),
        }
        assert!(class_is_denied("Keyboard"));
        assert!(class_is_denied("usbstor"));
        // A fresh dongle can report "Bluetooth" or no class — never denied.
        assert!(!class_is_denied("Bluetooth"));
        assert!(!class_is_denied("USBDevice"));
        assert!(!class_is_denied(""));
    }

    #[test]
    fn unknown_dongle_needs_the_opt_in() {
        let stranger = DeviceFacts {
            vid: 0x1111,
            pid: 0x2222,
            ..good_facts()
        };
        assert_eq!(
            evaluate_install_target(&stranger, false).unwrap_err(),
            InstallRejection::UnknownDongle {
                vid: 0x1111,
                pid: 0x2222
            }
        );
        let approval = evaluate_install_target(&stranger, true).unwrap();
        assert_eq!(approval.tier, None);
        assert!(approval.unknown_opt_in);
    }

    #[test]
    fn rejection_messages_are_actionable() {
        assert!(InstallRejection::UnknownDongle { vid: 1, pid: 2 }
            .message()
            .contains(UNKNOWN_DONGLE_OPT_IN));
        assert!(InstallRejection::Composite
            .message()
            .to_lowercase()
            .contains("combo"));
    }
}
