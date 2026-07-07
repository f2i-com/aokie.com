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
    /// v2-only. `install` (default) runs the WinUSB INF install;
    /// `remove-certs` walks LocalMachine\Root and
    /// LocalMachine\TrustedPublisher and deletes every Aokie-signed
    /// cert. Default lets a v1 dispatcher's job (no `mode` field) keep
    /// working through this helper without rebuilding both sides.
    #[serde(default = "default_job_mode")]
    mode: String,
    vid: u16,
    pid: u16,
    inf_path: PathBuf,
}

fn default_job_version() -> u32 {
    1
}

fn default_job_mode() -> String {
    "install".to_string()
}

const SUPPORTED_JOB_VERSION: u32 = 2;

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
        other => Err(format!(
            "unknown job mode {:?}; supported modes are 'install' and 'remove-certs'",
            other
        )),
    }
}

fn run_install(job: &DriverJob, job_path: &Path) -> Result<(), String> {
    validate_inf_path(&job.inf_path, job_path)?;

    let expected = aokie_dongle::winusb::hardware_id(job.vid, job.pid);
    log_line!(
        "[aokie-driver-helper] install job: vid=0x{:04x} pid=0x{:04x} inf={:?}",
        job.vid,
        job.pid,
        job.inf_path
    );
    // Device-presence check is informational only. install_package
    // succeeds even when the device is unplugged — it stages the INF +
    // signed cat into the driver store so the next plug-in binds
    // automatically. Mirroring libwdi's "no device, INF copied for
    // next time" behaviour avoids forcing the user to keep the dongle
    // plugged during the elevation prompt.
    match aokie_dongle::find_device(job.vid, job.pid)? {
        Some(device) if device.hardware_id.to_ascii_uppercase().contains(&expected) => {
            log_line!(
                "[aokie-driver-helper] target {} is currently plugged in",
                expected
            );
        }
        Some(device) => {
            log_line!(
                "[aokie-driver-helper] device hardware ID mismatch: expected {}, got {} — staging INF anyway",
                expected,
                device.hardware_id
            );
        }
        None => {
            log_line!(
                "[aokie-driver-helper] {} not currently plugged in — staging INF for next plug-in",
                expected
            );
        }
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
}
