#![cfg(target_os = "windows")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};


#[derive(Debug, serde::Deserialize)]
struct DriverJob {
    /// Bumped whenever the dispatcher and helper both need a coordinated
    /// schema change. The helper rejects unknown versions so a stale
    /// helper exe (e.g. left next to an old app build) refuses to run a
    /// job it can't parse cleanly.
    #[serde(default = "default_job_version")]
    version: u32,
    /// `install` (default) runs the WinUSB INF install; `remove-certs`
    /// walks LocalMachine\Root and LocalMachine\TrustedPublisher and
    /// deletes every Aokie-signed cert; `restore-driver` reverts the
    /// device to its in-box driver (AOK-DRIVER-001). Default lets a v1
    /// dispatcher's job (no `mode` field) keep working through this
    /// helper without rebuilding both sides.
    #[serde(default = "default_job_mode")]
    mode: String,
    vid: u16,
    pid: u16,
    inf_path: PathBuf,
    /// AOK-DRIVER-001: the device-instance id the unelevated dispatcher
    /// approved. The helper re-enumerates and refuses if the live
    /// target's instance id differs. Defaulted so a v1/v2 job (no field)
    /// still parses — a missing instance id just skips the exact-match
    /// check, leaving the catalog/class/composite re-validation in force.
    #[serde(default)]
    instance_id: String,
    /// AOK-DRIVER-001: SHA-256 of the INF the dispatcher approved. The
    /// helper recomputes the INF on disk and refuses on mismatch.
    #[serde(default)]
    inf_sha256: String,
}

fn default_job_version() -> u32 {
    1
}

fn default_job_mode() -> String {
    "install".to_string()
}

const SUPPORTED_JOB_VERSION: u32 = 3;

/// %TEMP%\aokie-driver-helper.log. Lazily initialised the first time
/// `log_line!` fires so a helper that bails early during arg parsing
/// still gets a record. None means we couldn't open the log (very
/// unwritable %TEMP%); writes degrade to stdout/stderr only.
static LOG_FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

fn log_file() -> Option<&'static Mutex<std::fs::File>> {
    LOG_FILE
        .get_or_init(|| {
            let path = std::env::temp_dir().join("aokie-driver-helper.log");
            // Truncate per-run: the dispatcher can read it after the
            // helper exits to surface failures; we don't want stale
            // lines from a previous install confusing diagnosis.
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .ok()
                .map(Mutex::new)
        })
        .as_ref()
}

macro_rules! log_line {
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        println!("{}", line);
        if let Some(lock) = log_file() {
            if let Ok(mut f) = lock.lock() {
                let _ = writeln!(f, "{}", line);
                let _ = f.flush();
            }
        }
    }};
}

macro_rules! log_err {
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        eprintln!("{}", line);
        if let Some(lock) = log_file() {
            if let Ok(mut f) = lock.lock() {
                let _ = writeln!(f, "{}", line);
                let _ = f.flush();
            }
        }
    }};
}

fn main() {
    let now = chrono::Local::now();
    log_line!(
        "[aokie-driver-helper] starting at {} (pid={})",
        now.format("%Y-%m-%d %H:%M:%S"),
        std::process::id()
    );
    if let Err(e) = run() {
        log_err!("[aokie-driver-helper] {}", e);
        std::process::exit(1);
    }
    log_line!("[aokie-driver-helper] done");
}

fn run() -> Result<(), String> {
    let job_path = parse_job_path()?;
    validate_job_path(&job_path)?;
    let job_json = std::fs::read_to_string(&job_path)
        .map_err(|e| format!("could not read job file {:?}: {}", job_path, e))?;
    let job: DriverJob = serde_json::from_str(&job_json)
        .map_err(|e| format!("could not parse job file {:?}: {}", job_path, e))?;

    if job.version != SUPPORTED_JOB_VERSION {
        return Err(format!(
            "unsupported job version {} (this helper speaks v{}); the app and \
             helper exe are out of sync — reinstall to refresh both",
            job.version, SUPPORTED_JOB_VERSION
        ));
    }

    match job.mode.as_str() {
        "install" => run_install(&job, &job_path),
        "remove-certs" => run_remove_certs(),
        "restore-driver" => run_restore(&job),
        other => Err(format!(
            "unknown job mode {:?}; supported modes are 'install', 'remove-certs' and 'restore-driver'",
            other
        )),
    }
}

fn run_install(job: &DriverJob, job_path: &Path) -> Result<(), String> {
    validate_inf_path(&job.inf_path, job_path)?;

    // AOK-DRIVER-001 — verify the job + INF were written by a trusted
    // principal (the elevated user, Administrators, or SYSTEM). This
    // closes the "a lower-privilege local process planted files in the
    // work dir and we're about to act on them elevated" race. Soft on a
    // security-API failure (logged) since the independent re-validation
    // below is the primary defence; hard on a definitively untrusted
    // owner.
    guard_trusted_owner(job_path);
    guard_trusted_owner(&job.inf_path);

    log_line!(
        "[aokie-driver-helper] install job: vid=0x{:04x} pid=0x{:04x} inf={:?}",
        job.vid,
        job.pid,
        job.inf_path
    );

    // AOK-DRIVER-001 — re-run the SAME install policy the unelevated
    // dispatcher ran, against the LIVE device set, INSIDE the elevation.
    // A tampered job that swapped in a keyboard / internal combo /
    // absent-or-unknown VID-PID is re-judged and refused here even if it
    // slipped past the unelevated gate. Unlike the old code this is NOT
    // "informational only": a device that fails policy stops the install.
    let device = aokie_dongle::evaluate_present_target(
        job.vid,
        job.pid,
        aokie_dongle::allow_unknown_dongle(),
    )
    .map_err(|e| format!("device policy refused this target inside the helper: {}", e))?;
    log_line!(
        "[aokie-driver-helper] target present + policy-approved: {} ({})",
        device.instance_id,
        device.description
    );

    // Exact device-instance match: the helper must bind the SAME
    // physical unit the operator approved, not a same-model dongle
    // swapped in between approval and elevation. Skipped only when the
    // job carried no instance id (a legacy v1/v2 job).
    if !instance_matches(&job.instance_id, &device.instance_id) {
        return Err(format!(
            "device instance changed since approval: job approved {:?} but the present \
             {:04x}:{:04x} is {:?} — refusing (unplug/replug or re-run install)",
            job.instance_id, job.vid, job.pid, device.instance_id
        ));
    }

    // Exact INF-bytes match: the helper installs the SAME INF the
    // dispatcher rendered + fingerprinted, so a swapped INF pointing at a
    // different driver payload can't ride in on a tampered job.
    if !job.inf_sha256.is_empty() {
        let actual = aokie_dongle::sha256_file(&job.inf_path)
            .map_err(|e| format!("could not hash INF for verification: {}", e))?;
        if !inf_hash_matches(&job.inf_sha256, &actual) {
            return Err(format!(
                "INF SHA-256 mismatch: job approved {} but {:?} hashes to {} — refusing",
                job.inf_sha256, job.inf_path, actual
            ));
        }
        log_line!("[aokie-driver-helper] INF hash verified ({}…)", &actual[..actual.len().min(16)]);
    }

    let result = aokie_dongle::winusb::install_package(&job.inf_path, job.vid, job.pid)?;
    if result.reboot_required {
        log_line!("[aokie-driver-helper] WinUSB installed; reboot may be required");
    } else {
        log_line!("[aokie-driver-helper] WinUSB installed");
    }

    Ok(())
}

/// Wipe every Aokie-signed cert from LocalMachine\Root and
/// LocalMachine\TrustedPublisher (R11-#4). Failures on either store are
/// returned to the dispatcher with a count so the operator can see how
/// far we got before bailing.
fn run_remove_certs() -> Result<(), String> {
    log_line!("[aokie-driver-helper] remove-certs job: scanning LocalMachine\\Root and LocalMachine\\TrustedPublisher");
    let mut total = 0usize;
    let mut errs: Vec<String> = Vec::new();
    for store in ["Root", "TrustedPublisher"] {
        match aokie_dongle::pki::remove_all_aokie_certs(store) {
            Ok(n) => {
                log_line!(
                    "[aokie-driver-helper] LocalMachine\\{}: removed {} Aokie cert(s)",
                    store,
                    n
                );
                total += n;
            }
            Err(e) => {
                log_err!("[aokie-driver-helper] LocalMachine\\{}: {}", store, e);
                errs.push(format!("{}: {}", store, e));
            }
        }
    }
    if !errs.is_empty() {
        return Err(format!(
            "removed {} cert(s) before a store failed: {}",
            total,
            errs.join("; ")
        ));
    }
    log_line!(
        "[aokie-driver-helper] remove-certs done — total {} cert(s) removed",
        total
    );
    Ok(())
}

/// AOK-DRIVER-001 restore path: revert the device to its in-box driver.
/// Re-validates the target against the live catalog policy first (so a
/// tampered restore job can't be aimed at an arbitrary device), then
/// hands off to the winusb restore routine.
fn run_restore(job: &DriverJob) -> Result<(), String> {
    log_line!(
        "[aokie-driver-helper] restore-driver job: vid=0x{:04x} pid=0x{:04x}",
        job.vid,
        job.pid
    );
    // Only ever restore a device that is itself a legitimate Aokie
    // target — never touch an arbitrary device's driver binding.
    let device = aokie_dongle::evaluate_present_target(
        job.vid,
        job.pid,
        aokie_dongle::allow_unknown_dongle(),
    )
    .map_err(|e| format!("device policy refused this restore target: {}", e))?;
    aokie_dongle::winusb::restore_inbox_driver(job.vid, job.pid)?;
    log_line!(
        "[aokie-driver-helper] restored in-box driver for {}",
        device.instance_id
    );
    Ok(())
}

/// Refuse the install if `path`'s owner is a principal we don't trust
/// (i.e. not the elevated user, Administrators, or SYSTEM). A
/// security-API failure is logged and treated as a soft pass — the
/// catalog/instance/INF re-validation is the primary defence, and we
/// don't want to brick installs on an exotic filesystem — but a
/// definitively untrusted owner aborts.
fn guard_trusted_owner(path: &Path) {
    match aokie_dongle::winusb::file_owner_is_trusted(path) {
        Ok(true) => {}
        Ok(false) => {
            log_err!(
                "[aokie-driver-helper] refusing: {:?} is owned by an untrusted principal \
                 (possible planted file)",
                path
            );
            std::process::exit(1);
        }
        Err(e) => {
            log_line!(
                "[aokie-driver-helper] owner check for {:?} inconclusive ({}) — continuing on \
                 catalog/instance/INF re-validation",
                path,
                e
            );
        }
    }
}

/// AOK-DRIVER-001: the live device instance must match the one the
/// dispatcher approved. An empty approved id means a legacy v1/v2 job
/// that predates instance pinning — the catalog/class/composite
/// re-validation still stands, so we don't fail those closed.
fn instance_matches(approved_instance: &str, live_instance: &str) -> bool {
    approved_instance.is_empty() || approved_instance.eq_ignore_ascii_case(live_instance)
}

/// AOK-DRIVER-001: the INF on disk must hash to the value the dispatcher
/// approved. An empty approved hash means a legacy job (skip the check).
fn inf_hash_matches(approved_hash: &str, actual_hash: &str) -> bool {
    approved_hash.is_empty() || approved_hash.eq_ignore_ascii_case(actual_hash)
}

fn parse_job_path() -> Result<PathBuf, String> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--job" {
            return args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| "--job requires a path".to_string());
        }
    }
    Err("usage: aokie-driver-helper --job <job.json>".to_string())
}

/// Defense in depth: the helper runs elevated and acts on whatever
/// path the dispatcher passed via `--job`. If a non-elevated process
/// somehow plants a job file (race the dispatcher's write, intercept
/// the ShellExecuteEx args), we'd otherwise trust it. Constrain the
/// job-file shape so a tampered argument can't point at an arbitrary
/// JSON on disk.
///
/// Rules:
/// - must be absolute (no relative-path traversal)
/// - filename must be exactly `aokie_driver_job.json` — that's the
///   one and only filename `installer.rs::run_helper_install` ever
///   writes. Any other name is suspicious.
/// - file must already exist (rejects a job path that points at a
///   parent directory we control but no job file)
fn validate_job_path(job_path: &Path) -> Result<(), String> {
    if !job_path.is_absolute() {
        return Err(format!("job path must be absolute, got {:?}", job_path));
    }
    let name = job_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("job path has no filename component: {:?}", job_path))?;
    if name != "aokie_driver_job.json" {
        return Err(format!(
            "unexpected job filename {:?} — must be aokie_driver_job.json",
            name
        ));
    }
    if !job_path.is_file() {
        return Err(format!(
            "job file does not exist or is not a file: {:?}",
            job_path
        ));
    }
    Ok(())
}

/// The helper passes `inf_path` straight to the Win32 SetupAPI which
/// will install whichever driver INF the JSON points at. Constrain
/// the INF path so a tampered job file can't redirect us to install
/// an arbitrary driver from somewhere else on disk.
///
/// Rules (all enforced against the post-canonicalisation path so a
/// `..` traversal can't slip past):
/// - must be absolute
/// - must have a `.inf` extension (case-insensitive)
/// - must be a real file
/// - must be in the **same directory** as the job file. The
///   dispatcher (`installer.rs::run_helper_install`) writes the INF
///   and the job JSON into the same `work_dir` together, so a
///   legitimate inf_path always sits next to job_path. Anything else
///   means the JSON was tampered with.
fn validate_inf_path(inf_path: &Path, job_path: &Path) -> Result<(), String> {
    if !inf_path.is_absolute() {
        return Err(format!("inf_path must be absolute, got {:?}", inf_path));
    }
    let ext = inf_path
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| format!("inf_path has no extension: {:?}", inf_path))?;
    if !ext.eq_ignore_ascii_case("inf") {
        return Err(format!(
            "inf_path must end in .inf, got extension {:?}",
            ext
        ));
    }
    if !inf_path.is_file() {
        return Err(format!(
            "inf_path does not exist or is not a file: {:?}",
            inf_path
        ));
    }
    // Same-directory check uses canonicalised paths so that
    // forward/back slash mixing or short-name (8.3) variants don't
    // false-negative.
    let inf_dir = inf_path
        .canonicalize()
        .map_err(|e| format!("could not canonicalise inf_path {:?}: {}", inf_path, e))?
        .parent()
        .ok_or_else(|| format!("inf_path has no parent directory: {:?}", inf_path))?
        .to_path_buf();
    let job_dir = job_path
        .canonicalize()
        .map_err(|e| format!("could not canonicalise job_path {:?}: {}", job_path, e))?
        .parent()
        .ok_or_else(|| format!("job_path has no parent directory: {:?}", job_path))?
        .to_path_buf();
    if inf_dir != job_dir {
        return Err(format!(
            "inf_path {:?} is not in the same directory as job_path {:?} — \
             refusing to install an INF from outside the job's work_dir",
            inf_path, job_path
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        std::fs::write(p, "").unwrap();
    }

    #[test]
    fn validate_job_path_rejects_relative_path() {
        let err = validate_job_path(Path::new("aokie_driver_job.json")).unwrap_err();
        assert!(err.contains("must be absolute"), "got: {}", err);
    }

    #[test]
    fn validate_job_path_rejects_wrong_filename() {
        let dir = std::env::temp_dir().join(format!(
            "aokie-driver-helper-test-{}-{}",
            std::process::id(),
            "wrongname"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("evil.json");
        touch(&bad);
        let err = validate_job_path(&bad).unwrap_err();
        assert!(
            err.contains("must be aokie_driver_job.json"),
            "got: {}",
            err
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_job_path_accepts_valid() {
        let dir = std::env::temp_dir().join(format!(
            "aokie-driver-helper-test-{}-{}",
            std::process::id(),
            "valid"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("aokie_driver_job.json");
        touch(&p);
        validate_job_path(&p).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_inf_path_rejects_extension_mismatch() {
        let dir = std::env::temp_dir().join(format!(
            "aokie-driver-helper-test-{}-{}",
            std::process::id(),
            "ext"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let job = dir.join("aokie_driver_job.json");
        let inf = dir.join("aokie_winusb.txt");
        touch(&job);
        touch(&inf);
        let err = validate_inf_path(&inf, &job).unwrap_err();
        assert!(err.contains("must end in .inf"), "got: {}", err);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_inf_path_rejects_outside_job_dir() {
        let dir_job = std::env::temp_dir().join(format!(
            "aokie-driver-helper-test-{}-{}",
            std::process::id(),
            "outside-job"
        ));
        let dir_inf = std::env::temp_dir().join(format!(
            "aokie-driver-helper-test-{}-{}",
            std::process::id(),
            "outside-inf"
        ));
        std::fs::create_dir_all(&dir_job).unwrap();
        std::fs::create_dir_all(&dir_inf).unwrap();
        let job = dir_job.join("aokie_driver_job.json");
        let inf = dir_inf.join("aokie_winusb.inf");
        touch(&job);
        touch(&inf);
        let err = validate_inf_path(&inf, &job).unwrap_err();
        assert!(err.contains("not in the same directory"), "got: {}", err);
        let _ = std::fs::remove_dir_all(&dir_job);
        let _ = std::fs::remove_dir_all(&dir_inf);
    }

    #[test]
    fn validate_inf_path_accepts_sibling() {
        let dir = std::env::temp_dir().join(format!(
            "aokie-driver-helper-test-{}-{}",
            std::process::id(),
            "sibling"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let job = dir.join("aokie_driver_job.json");
        let inf = dir.join("aokie_winusb.inf");
        touch(&job);
        touch(&inf);
        validate_inf_path(&inf, &job).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- AOK-DRIVER-001 ----

    #[test]
    fn instance_match_is_case_insensitive_and_exact() {
        assert!(instance_matches(
            "USB\\VID_0A5C&PID_21EC\\00198600226C",
            "usb\\vid_0a5c&pid_21ec\\00198600226c"
        ));
        assert!(!instance_matches(
            "USB\\VID_0A5C&PID_21EC\\00198600226C",
            "USB\\VID_0A5C&PID_21EC\\DEADBEEF0000"
        ));
    }

    #[test]
    fn empty_approved_instance_skips_the_check() {
        // A legacy v1/v2 job carried no instance id — don't fail it
        // closed; the catalog/class/composite re-validation still runs.
        assert!(instance_matches("", "USB\\VID_0A5C&PID_21EC\\ANY"));
    }

    #[test]
    fn inf_hash_match_is_case_insensitive() {
        let h = "ABCD1234";
        assert!(inf_hash_matches(h, "abcd1234"));
        assert!(!inf_hash_matches(h, "0000ffff"));
        assert!(inf_hash_matches("", "anything")); // legacy job → skip
    }

    #[test]
    fn v2_job_without_new_fields_still_parses() {
        // Back-compat: a job missing instance_id / inf_sha256 deserializes
        // with empty defaults (which the match helpers treat as skip).
        let json = r#"{"version":2,"mode":"install","vid":2652,"pid":8684,
                       "inf_path":"C:\\x\\aokie_winusb_bluetooth.inf"}"#;
        let job: DriverJob = serde_json::from_str(json).unwrap();
        assert_eq!(job.version, 2);
        assert_eq!(job.mode, "install");
        assert!(job.instance_id.is_empty());
        assert!(job.inf_sha256.is_empty());
    }

    #[test]
    fn v3_job_round_trips_the_new_fields() {
        let json = r#"{"version":3,"mode":"install","vid":2652,"pid":8684,
                       "inf_path":"C:\\x\\aokie_winusb_bluetooth.inf",
                       "instance_id":"USB\\VID_0A5C&PID_21EC\\00198600226C",
                       "inf_sha256":"deadbeef"}"#;
        let job: DriverJob = serde_json::from_str(json).unwrap();
        assert_eq!(job.version, 3);
        assert_eq!(job.instance_id, "USB\\VID_0A5C&PID_21EC\\00198600226C");
        assert_eq!(job.inf_sha256, "deadbeef");
    }
}
