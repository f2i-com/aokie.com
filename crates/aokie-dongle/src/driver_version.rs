//! The `DriverVer` an Aokie WinUSB INF carries: `MM/DD/YYYY,2.YY.MMDD.HHMM`.
//!
//! Windows reads each of the four version numbers as a 16-bit field (0 to 65535), and between two packages of
//! the same rank for a device it takes the one with the later date, then the higher version. The old form put
//! YYYYMMDD (20261018) in the last field, more than 16 bits hold, and it changed once a day: a second install on
//! the same day rendered, byte for byte, the package already in the driver store, whose catalog was signed by the
//! certificate the first install made (which "Remove signing certificates" may since have deleted). Here the
//! year, the month and day, and the hour and minute each fit their field, so an install in a later minute
//! renders a newer package.
//!
//! The dispatcher picks the value once, renders the INF with it and hands it to the elevated helper in the job;
//! the helper renders its own copy with the same value instead of reading the clock again, so a UAC prompt
//! answered after midnight (or a minute later) still matches the INF that was approved. The clock is UTC's, which
//! never goes back an hour. Pure, so it is tested on every system; only Windows installs a driver.

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};

/// The major version every Aokie WinUSB package has carried.
const MAJOR: u16 = 2;

/// The `DriverVer` of a package rendered at `at` (UTC: see [`driver_ver_now`]).
pub fn driver_ver_at(at: NaiveDateTime) -> String {
    let (minor, build, revision) = version_fields(at.date(), at.hour(), at.minute());
    format!(
        "{:02}/{:02}/{:04},{MAJOR}.{minor}.{build}.{revision}",
        at.month(),
        at.day(),
        at.year()
    )
}

/// The `DriverVer` of a package rendered now, in UTC (review 3 #33): local
/// time goes back an hour when summer time ends, and a package rendered in
/// that hour would read as older than the one rendered just before it, which
/// Windows then keeps.
pub fn driver_ver_now() -> String {
    driver_ver_at(chrono::Utc::now().naive_utc())
}

/// Today as [`driver_ver_now`] dates it (UTC), for the helper's check of a job's value
/// ([`check_job_driver_ver`]).
pub fn driver_ver_today() -> NaiveDate {
    chrono::Utc::now().date_naive()
}

/// YY, MMDD and HHMM (a clock before 2000 counts as 2000: the field cannot go below 0).
fn version_fields(date: NaiveDate, hour: u32, minute: u32) -> (u16, u16, u16) {
    let minor = (date.year() - 2000).clamp(0, u16::MAX as i32) as u16;
    let build = (date.month() * 100 + date.day()) as u16;
    let revision = (hour * 100 + minute) as u16;
    (minor, build, revision)
}

/// The date and the four version numbers of a `DriverVer` value, or why Windows would not read it as one.
pub fn parse_driver_ver(value: &str) -> Result<(NaiveDate, [u16; 4]), String> {
    let (date, version) = value
        .split_once(',')
        .ok_or_else(|| format!("DriverVer {value:?} has no version after its date"))?;
    let date = NaiveDate::parse_from_str(date.trim(), "%m/%d/%Y")
        .map_err(|e| format!("DriverVer {value:?} has no MM/DD/YYYY date: {e}"))?;
    let fields: Vec<&str> = version.trim().split('.').collect();
    if fields.len() != 4 {
        return Err(format!("DriverVer {value:?} does not have four version numbers"));
    }
    let mut numbers = [0u16; 4];
    for (slot, field) in numbers.iter_mut().zip(&fields) {
        if field.is_empty() || !field.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("DriverVer {value:?}: {field:?} is not a number"));
        }
        *slot = field
            .parse::<u16>()
            .map_err(|_| format!("DriverVer {value:?}: {field} does not fit a 16-bit field (0 to 65535)"))?;
    }
    Ok((date, numbers))
}

/// The elevated helper's check of the `driver_ver` an install job brings: Windows reads it, it is the form
/// [`driver_ver_at`] renders for its own date, and that date is today's (a day either side, for a prompt
/// answered across midnight or a clock zone apart). The rest of the INF is the helper's own renderer's, so this
/// is the one value the job chooses.
pub fn check_job_driver_ver(value: &str, today: NaiveDate) -> Result<(), String> {
    let (date, numbers) = parse_driver_ver(value)?;
    let (minor, build, _) = version_fields(date, 0, 0);
    let revision = numbers[3];
    if numbers[0] != MAJOR
        || numbers[1] != minor
        || numbers[2] != build
        || revision / 100 > 23
        || revision % 100 > 59
    {
        return Err(format!(
            "DriverVer {value:?} is not the {MAJOR}.YY.MMDD.HHMM form Aokie renders for its date"
        ));
    }
    let days = (date - today).num_days();
    if !(-1..=1).contains(&days) {
        return Err(format!("DriverVer {value:?} is dated {date}, not today ({today})"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, min, 0).unwrap()
    }

    #[test]
    fn renders_date_and_four_16_bit_fields() {
        assert_eq!(driver_ver_at(at(2026, 10, 18, 23, 59)), "10/18/2026,2.26.1018.2359");
        assert_eq!(driver_ver_at(at(2026, 1, 5, 0, 7)), "01/05/2026,2.26.105.7");
        for value in [at(2026, 12, 31, 23, 59), at(2099, 12, 31, 23, 59), at(1999, 1, 1, 0, 0)] {
            parse_driver_ver(&driver_ver_at(value)).unwrap();
        }
    }

    #[test]
    fn a_later_minute_renders_a_newer_package() {
        // Windows compares the date, then the version field by field.
        let key = |value: NaiveDateTime| parse_driver_ver(&driver_ver_at(value)).unwrap();
        let order = [
            at(2026, 10, 18, 9, 0),
            at(2026, 10, 18, 9, 1),
            at(2026, 10, 18, 23, 59),
            at(2026, 10, 19, 0, 0),
            at(2026, 12, 31, 23, 59),
            at(2027, 1, 1, 0, 0),
        ];
        for pair in order.windows(2) {
            assert!(key(pair[0]) < key(pair[1]), "{:?} then {:?}", pair[0], pair[1]);
        }
        // Newer than every package the old form rendered (2.2.0.YYYYMMDD; Windows kept 16 bits of it at best).
        assert!(key(at(2026, 10, 18, 0, 0)).1 > [2, 2, 0, u16::MAX]);
    }

    #[test]
    fn refuses_what_windows_cannot_read() {
        // The old form: its last field is more than 16 bits.
        assert!(parse_driver_ver("07/15/2026,2.2.0.20260715").unwrap_err().contains("16-bit"));
        assert!(parse_driver_ver("07/15/2026").is_err());
        assert!(parse_driver_ver("2026-07-15,2.26.715.0").is_err());
        assert!(parse_driver_ver("07/15/2026,2.26.715").is_err());
        assert!(parse_driver_ver("07/15/2026,2.26.715.-1").is_err());
        assert!(parse_driver_ver("07/15/2026,2.26.715.0x10").is_err());
    }

    #[test]
    fn a_job_brings_todays_value_in_aokies_form() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 18).unwrap();
        check_job_driver_ver("10/18/2026,2.26.1018.2359", today).unwrap();
        // A UAC prompt answered after midnight, or rendered a clock zone ahead.
        check_job_driver_ver("10/17/2026,2.26.1017.2359", today).unwrap();
        check_job_driver_ver("10/19/2026,2.26.1019.0", today).unwrap();
        assert!(check_job_driver_ver("10/15/2026,2.26.1015.1200", today).unwrap_err().contains("not today"));
        assert!(check_job_driver_ver("10/21/2026,2.26.1021.1200", today).is_err());
        // Well formed for Windows, but not what Aokie renders for that date.
        assert!(check_job_driver_ver("10/18/2026,9.26.1018.1200", today).is_err());
        assert!(check_job_driver_ver("10/18/2026,2.27.1018.1200", today).is_err());
        assert!(check_job_driver_ver("10/18/2026,2.26.1019.1200", today).is_err());
        assert!(check_job_driver_ver("10/18/2026,2.26.1018.2460", today).is_err());
        assert!(check_job_driver_ver("10/18/2026,2.26.1018.2400", today).is_err());
    }

    /// The dispatcher's value and the helper's today are read off one clock, UTC's (review 3 #33).
    #[test]
    fn the_value_rendered_now_is_dated_today_on_the_same_clock() {
        check_job_driver_ver(&driver_ver_now(), driver_ver_today()).unwrap();
        let (date, _) = parse_driver_ver(&driver_ver_now()).unwrap();
        assert!((date - chrono::Utc::now().date_naive()).num_days().abs() <= 1);
    }

    /// The static package submitted for Microsoft signing: Windows reads its DriverVer, and it covers exactly the
    /// catalog's dongles (never a computer's own Bluetooth). Checked here so it runs on every system; the Windows
    /// build checks the same coverage through its own hardware-id renderer.
    #[test]
    fn the_static_release_inf_is_readable_and_names_only_catalog_dongles() {
        let inf = include_str!("../../../drivers/winusb/aokie_winusb_bluetooth.inf");
        let driver_ver = inf
            .lines()
            .find_map(|line| line.trim().strip_prefix("DriverVer="))
            .expect("the static INF has a DriverVer");
        parse_driver_ver(driver_ver).unwrap();
        let upper = inf.to_ascii_uppercase();
        let named: std::collections::BTreeSet<String> = upper
            .match_indices("USB\\VID_")
            .map(|(at, _)| upper[at..at + 21].to_string())
            .collect();
        let catalog: std::collections::BTreeSet<String> = aokie_core::dongle_catalog::DEFAULT_CATALOG
            .iter()
            .map(|d| format!("USB\\VID_{:04X}&PID_{:04X}", d.vid, d.pid))
            .collect();
        assert_eq!(named, catalog);
    }
}
