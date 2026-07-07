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
}
