#![cfg(target_os = "windows")]

use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use sha2::{Digest, Sha256};

use windows_sys::Win32::Foundation::{GetLastError, LocalFree, S_OK};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::Win32::UI::Shell::{SHGetFolderPathW, CSIDL_COMMON_APPDATA};

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
    /// approved. DRIVER-001 made it MANDATORY for install/restore — the
    /// helper refuses a job without it (no more "empty skips the check").
    #[serde(default)]
    instance_id: String,
    /// AOK-DRIVER-001: SHA-256 of the INF the dispatcher approved.
    /// DRIVER-001: mandatory (64 hex chars) on install jobs; the helper
    /// recomputes the INF on disk and refuses on mismatch.
    #[serde(default)]
    inf_sha256: String,
    /// AK-DRV-02: digest of the adjacent signed catalog. Production compares
    /// this against a digest compiled into this signed helper.
    #[serde(default)]
    cat_sha256: String,
    /// DRIVER-001: the expected hardware id (`USB\VID_xxxx&PID_xxxx`).
    /// Mandatory on install jobs and must equal the id the helper derives
    /// from vid/pid itself — a job whose fields disagree is refused.
    #[serde(default)]
    hardware_id: String,
    /// DRIVER-001: whether the dispatcher authorised DEVELOPER self-signing
    /// (generating a local catalog-signing cert and trusting it machine-wide).
    /// Production dispatchers write false; the install then requires a
    /// shipped signed catalog and never mints a certificate.
    #[serde(default)]
    allow_dev_self_sign: bool,
}

fn default_job_version() -> u32 {
    1
}

fn default_job_mode() -> String {
    "install".to_string()
}

/// v5 (AK-DRV-02): `cat_sha256` binds the signed catalog, and install
/// package bytes move through locked handles into an admin-only directory.
///
/// v4 (DRIVER-001): `instance_id`/`inf_sha256` became mandatory for
/// install, `hardware_id` + `allow_dev_self_sign` were added, restore
/// jobs pin the instance id, and exact package checks fail CLOSED. Exact
/// version match — a stale helper or stale dispatcher refuses to run.
const SUPPORTED_JOB_VERSION: u32 = 5;

#[cfg(all(not(debug_assertions), not(feature = "managed-beta-driver")))]
const EXPECTED_INF_SHA256: Option<&str> = Some(env!(
    "AOKIE_EXPECTED_DRIVER_INF_SHA256",
    "Release helpers require the exact Microsoft-signed package INF digest"
));
#[cfg(any(debug_assertions, feature = "managed-beta-driver"))]
const EXPECTED_INF_SHA256: Option<&str> = option_env!("AOKIE_EXPECTED_DRIVER_INF_SHA256");

#[cfg(all(not(debug_assertions), not(feature = "managed-beta-driver")))]
const EXPECTED_CAT_SHA256: Option<&str> = Some(env!(
    "AOKIE_EXPECTED_DRIVER_CAT_SHA256",
    "Release helpers require the exact Microsoft-signed package CAT digest"
));
#[cfg(any(debug_assertions, feature = "managed-beta-driver"))]
const EXPECTED_CAT_SHA256: Option<&str> = option_env!("AOKIE_EXPECTED_DRIVER_CAT_SHA256");

/// A job-file boolean alone must never unlock a privileged trust-store
/// mutation. Only debug helpers and the deliberately distinct managed-beta
/// release flavour contain the self-signing path.
const fn self_sign_capable(debug_build: bool, managed_beta_build: bool) -> bool {
    debug_build || managed_beta_build
}

const SELF_SIGN_CAPABLE_HELPER: bool = self_sign_capable(
    cfg!(debug_assertions),
    cfg!(feature = "managed-beta-driver"),
);

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
    // AK-DRV-02: read the untrusted request once through a handle that denies
    // sharing. After parsing, no later operation reopens or trusts the job.
    let job_json = String::from_utf8(read_locked(&job_path, 64 * 1024)?)
        .map_err(|e| format!("job file {:?} is not UTF-8: {}", job_path, e))?;
    let job: DriverJob = serde_json::from_str(&job_json)
        .map_err(|e| format!("could not parse job file {:?}: {}", job_path, e))?;

    if job.version != SUPPORTED_JOB_VERSION {
        return Err(format!(
            "unsupported job version {} (this helper speaks v{}); the app and \
             helper exe are out of sync — reinstall to refresh both",
            job.version, SUPPORTED_JOB_VERSION
        ));
    }

    // The request remains untrusted even though it was read through a locked handle.
    // Every privileged mode therefore re-authorizes its exact target below.
    transaction_log(
        &job.mode,
        &format!(
            "job accepted: vid=0x{:04x} pid=0x{:04x} instance={:?}",
            job.vid, job.pid, job.instance_id
        ),
    );

    let result = match job.mode.as_str() {
        "install" => run_install(&job, &job_path),
        "remove-certs" => run_remove_certs(),
        "restore-driver" => run_restore(&job),
        other => Err(format!(
            "unknown job mode {:?}; supported modes are 'install', 'remove-certs' and 'restore-driver'",
            other
        )),
    };
    match &result {
        Ok(()) => transaction_log(&job.mode, "completed"),
        Err(e) => transaction_log(&job.mode, &format!("refused/failed: {}", e)),
    }
    result
}

/// DRIVER-001: append-only transaction journal of every elevated driver
/// mutation (and refusal), machine-wide so it survives per-user temp
/// cleanup. One JSON line per entry: what mode ran, when, and what it did —
/// the durable record restore/uninstall work can be audited against.
/// Best-effort by design: journaling must never turn into a way to block
/// (or be blocked from) an otherwise-valid job, so failures degrade to the
/// per-run log only.
fn transaction_log(mode: &str, detail: &str) {
    let dir = match program_data_dir() {
        Ok(path) => path.join("Aokie"),
        Err(error) => {
            log_err!(
                "[aokie-driver-helper] transaction journal unavailable: {}",
                error
            );
            return;
        }
    };
    let entry = format!(
        "{{\"at\":{:?},\"pid\":{},\"mode\":{:?},\"detail\":{:?}}}",
        chrono::Local::now().to_rfc3339(),
        std::process::id(),
        mode,
        detail
    );
    let write = std::fs::create_dir_all(&dir).and_then(|()| {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("driver-transactions.jsonl"))?;
        writeln!(f, "{}", entry)?;
        f.flush()
    });
    if let Err(e) = write {
        log_err!(
            "[aokie-driver-helper] transaction journal write failed ({}): {}",
            e,
            entry
        );
    }
}

/// DRIVER-001: install jobs carry every exact-target field, non-empty and
/// well-formed — a job that omits any of them is refused before elevation
/// does anything. (The old empty-field "skip the check" tolerance is gone.)
fn validate_install_job_fields(job: &DriverJob) -> Result<(), String> {
    if job.instance_id.trim().is_empty() {
        return Err(
            "install job has no device instance id — the dispatcher must pin the exact \
             approved device; refusing"
                .to_string(),
        );
    }
    let hash = job.inf_sha256.trim();
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "install job's inf_sha256 {:?} is not a 64-hex SHA-256 — the dispatcher must \
             fingerprint the exact approved INF; refusing",
            job.inf_sha256
        ));
    }
    let self_sign = authorized_self_sign(job)?;
    let cat_hash = job.cat_sha256.trim();
    if !self_sign && (cat_hash.len() != 64 || !cat_hash.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(
            "production install job has no valid cat_sha256; refusing an unbound driver package"
                .to_string(),
        );
    }
    if !self_sign {
        if let Some(expected_inf) = EXPECTED_INF_SHA256 {
            if !expected_inf.eq_ignore_ascii_case(hash) {
                return Err(format!(
                    "INF digest is not the package pinned into this helper: expected {}, got {}",
                    expected_inf, hash
                ));
            }
        } else if !cfg!(debug_assertions) {
            return Err(
                "this helper has no production INF pin; refusing a shipped driver package"
                    .to_string(),
            );
        }
        if let Some(expected_cat) = EXPECTED_CAT_SHA256 {
            if !expected_cat.eq_ignore_ascii_case(cat_hash) {
                return Err(format!(
                    "catalog digest is not the package pinned into this helper: expected {}, got {}",
                    expected_cat, cat_hash
                ));
            }
        } else if !cfg!(debug_assertions) {
            return Err(
                "this helper has no production catalog pin; refusing a shipped driver package"
                    .to_string(),
            );
        }
    }
    let expected_hwid = aokie_dongle::winusb::hardware_id(job.vid, job.pid);
    if !job.hardware_id.eq_ignore_ascii_case(&expected_hwid) {
        return Err(format!(
            "install job's hardware_id {:?} does not match the id derived from \
             vid/pid ({:?}) — inconsistent job; refusing",
            job.hardware_id, expected_hwid
        ));
    }
    Ok(())
}

fn authorized_self_sign(job: &DriverJob) -> Result<bool, String> {
    if job.allow_dev_self_sign && !SELF_SIGN_CAPABLE_HELPER {
        return Err(
            "self-signing was requested, but this is a standard production helper; install the \
             managed-beta build or use the Microsoft-signed driver package"
                .to_string(),
        );
    }
    Ok(job.allow_dev_self_sign)
}

/// Read an attacker-writable input through a handle that denies read, write,
/// and delete sharing. A replacement race therefore either loses before the
/// open (and is caught by the pinned digest) or fails while this read runs.
fn read_locked(path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
    use std::os::windows::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
        .map_err(|e| format!("could not open {:?} without sharing: {}", path, e))?;
    let len = file
        .metadata()
        .map_err(|e| format!("could not stat {:?}: {}", path, e))?
        .len();
    if len > max_bytes {
        return Err(format!(
            "refusing oversized input {:?}: {} bytes exceeds {}",
            path, len, max_bytes
        ));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("could not read {:?}: {}", path, e))?;
    Ok(bytes)
}

struct PrivilegedPackage {
    root: PathBuf,
    inf_path: PathBuf,
    cat_path: Option<PathBuf>,
}

impl PrivilegedPackage {
    fn copy_from(job: &DriverJob) -> Result<Self, String> {
        let self_sign = authorized_self_sign(job)?;
        let root = create_admin_only_staging_dir()?;
        let inf_path = root.join(aokie_dongle::winusb::INF_NAME);
        let mut package = Self {
            root,
            inf_path,
            cat_path: None,
        };

        copy_locked_verified(
            &job.inf_path,
            &package.inf_path,
            &job.inf_sha256,
            4 * 1024 * 1024,
        )?;

        let source_cat = job.inf_path.with_file_name(aokie_dongle::winusb::CAT_NAME);
        if source_cat.is_file() {
            let cat_path = package.root.join(aokie_dongle::winusb::CAT_NAME);
            copy_locked_verified(&source_cat, &cat_path, &job.cat_sha256, 16 * 1024 * 1024)?;
            package.cat_path = Some(cat_path);
        } else if !self_sign {
            return Err(
                "the Microsoft-signed catalog is missing; refusing production install".into(),
            );
        }
        Ok(package)
    }
}

impl Drop for PrivilegedPackage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn copy_locked_verified(
    source: &Path,
    destination: &Path,
    expected_sha256: &str,
    max_bytes: u64,
) -> Result<(), String> {
    use std::os::windows::fs::OpenOptionsExt;

    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(format!("no valid pinned digest for {:?}", source));
    }
    let mut input = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(source)
        .map_err(|e| format!("open package file {:?} without sharing: {}", source, e))?;
    let len = input
        .metadata()
        .map_err(|e| format!("stat package file {:?}: {}", source, e))?
        .len();
    if len == 0 || len > max_bytes {
        return Err(format!(
            "package file {:?} has invalid size {}",
            source, len
        ));
    }
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .open(destination)
        .map_err(|e| format!("create privileged copy {:?}: {}", destination, e))?;
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|e| format!("read package file {:?}: {}", source, e))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|e| format!("write privileged copy {:?}: {}", destination, e))?;
        hasher.update(&buffer[..read]);
        copied += read as u64;
    }
    output
        .sync_all()
        .map_err(|e| format!("sync privileged copy {:?}: {}", destination, e))?;
    if copied != len {
        return Err(format!("short package copy for {:?}", source));
    }
    let actual = format!("{:x}", hasher.finalize());
    if !expected_sha256.eq_ignore_ascii_case(&actual) {
        return Err(format!(
            "package digest mismatch for {:?}: expected {}, got {}",
            source, expected_sha256, actual
        ));
    }
    Ok(())
}

/// Create a unique leaf directly under ProgramData with a protected DACL:
/// full control for SYSTEM and Administrators only, no inherited user ACEs.
fn create_admin_only_staging_dir() -> Result<PathBuf, String> {
    use std::os::windows::ffi::OsStrExt;

    // Never trust an inherited ProgramData environment variable at an
    // elevation boundary. Resolve the machine folder through Shell32.
    let program_data = program_data_dir()?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("system clock: {}", e))?
        .as_nanos();
    let sddl: Vec<u16> = "D:P(A;;FA;;;SY)(A;;FA;;;BA)"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(format!(
            "create protected staging security descriptor: Win32 error {}",
            unsafe { GetLastError() }
        ));
    }
    let mut security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let mut result = None;
    for sequence in 0..32_u32 {
        let path = program_data.join(format!(
            "AokieDriverStaging-{}-{nonce:x}-{sequence}",
            std::process::id()
        ));
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        if unsafe { CreateDirectoryW(wide.as_ptr(), &mut security) } != 0 {
            result = Some(Ok(path));
            break;
        }
        let error = unsafe { GetLastError() };
        if error != 183 {
            result = Some(Err(format!(
                "create protected staging directory: Win32 error {}",
                error
            )));
            break;
        }
    }
    unsafe {
        LocalFree(descriptor as _);
    }
    result.unwrap_or_else(|| Err("could not allocate a unique protected staging directory".into()))
}

fn program_data_dir() -> Result<PathBuf, String> {
    let mut buffer = [0_u16; 260];
    let result = unsafe {
        SHGetFolderPathW(
            std::ptr::null_mut(),
            CSIDL_COMMON_APPDATA as i32,
            std::ptr::null_mut(),
            0,
            buffer.as_mut_ptr(),
        )
    };
    if result != S_OK {
        return Err(format!(
            "resolve the machine ProgramData folder: HRESULT 0x{:08x}",
            result as u32
        ));
    }
    let len = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    if len == 0 {
        return Err("the machine ProgramData folder is empty".to_string());
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer[..len])))
}

fn run_install(job: &DriverJob, job_path: &Path) -> Result<(), String> {
    validate_inf_path(&job.inf_path, job_path)?;

    // DRIVER-001: every exact-target field is mandatory and well-formed —
    // there is no legacy "empty field skips the check" path anymore.
    validate_install_job_fields(job)?;
    let self_sign = authorized_self_sign(job)?;

    // AK-DRV-02: the source directory remains attacker-controlled. The INF
    // and CAT are copied through no-sharing handles into a fresh admin-only
    // staging directory and checked against their release-pinned digests
    // before any SetupAPI call below.
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
    // swapped in between approval and elevation.
    if !instance_matches(&job.instance_id, &device.instance_id) {
        return Err(format!(
            "device instance changed since approval: job approved {:?} but the present \
             {:04x}:{:04x} is {:?} — refusing (unplug/replug or re-run install)",
            job.instance_id, job.vid, job.pid, device.instance_id
        ));
    }

    // Managed-beta packages are rendered per selected VID/PID and therefore
    // cannot carry one compile-time INF digest. Reconstruct the only INF we
    // are willing to elevate from trusted helper code and live device facts,
    // then demand byte-for-byte equality. This retains the important property
    // that an unelevated process cannot add arbitrary services/co-installers.
    if self_sign {
        let supplied = read_locked(&job.inf_path, 4 * 1024 * 1024)?;
        let expected =
            aokie_dongle::winusb::WinusbPackage::new(job.vid, job.pid, &device.description)
                .render_inf();
        if supplied != expected.as_bytes() {
            return Err(
                "managed-beta INF differs from the helper's trusted renderer for the selected \
                 live dongle; refusing"
                    .to_string(),
            );
        }
        let rendered_hash = format!("{:x}", Sha256::digest(expected.as_bytes()));
        if !rendered_hash.eq_ignore_ascii_case(&job.inf_sha256) {
            return Err(
                "managed-beta INF digest does not match the helper-rendered package; refusing"
                    .to_string(),
            );
        }
    }

    // Exact INF-bytes match: the helper installs the SAME INF the
    // dispatcher rendered + fingerprinted, so a swapped INF pointing at a
    // different driver payload can't ride in on a tampered job. The hash
    // is re-verified HERE, immediately before mutation — the field itself
    // was already format-validated above.
    let package = PrivilegedPackage::copy_from(job)?;
    let actual = aokie_dongle::sha256_file(&package.inf_path)
        .map_err(|e| format!("could not re-hash privileged INF: {}", e))?;
    if !inf_hash_matches(&job.inf_sha256, &actual) {
        return Err(format!(
            "INF SHA-256 mismatch: job approved {} but {:?} hashes to {} — refusing",
            job.inf_sha256, job.inf_path, actual
        ));
    }
    log_line!(
        "[aokie-driver-helper] INF hash verified ({}…)",
        &actual[..actual.len().min(16)]
    );

    if !self_sign {
        let cat_path = package
            .cat_path
            .as_ref()
            .ok_or_else(|| "verified production catalog disappeared before install".to_string())?;
        let expected = EXPECTED_CAT_SHA256
            .ok_or_else(|| "production catalog pin is missing from this helper".to_string())?;
        let cat_actual = aokie_dongle::sha256_file(cat_path)
            .map_err(|e| format!("could not re-hash privileged catalog: {}", e))?;
        if !expected.eq_ignore_ascii_case(&cat_actual) {
            return Err("privileged catalog changed after verified copy; refusing".to_string());
        }
    }

    transaction_log(
        "install",
        &format!(
            "staging driver: instance={} inf={:?} inf_sha256={} dev_self_sign={}",
            device.instance_id, package.inf_path, actual, self_sign
        ),
    );
    let result =
        aokie_dongle::winusb::install_package(&package.inf_path, job.vid, job.pid, self_sign)?;
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
    // DRIVER-001: restore pins the exact approved instance too — a swapped
    // same-model device between approval and elevation is refused.
    if job.instance_id.trim().is_empty() {
        return Err(
            "restore job has no device instance id — the dispatcher must pin the exact \
             approved device; refusing"
                .to_string(),
        );
    }
    // Only ever restore a device that is itself a legitimate Aokie
    // target — never touch an arbitrary device's driver binding.
    let device = aokie_dongle::evaluate_present_target(
        job.vid,
        job.pid,
        aokie_dongle::allow_unknown_dongle(),
    )
    .map_err(|e| format!("device policy refused this restore target: {}", e))?;
    if !instance_matches(&job.instance_id, &device.instance_id) {
        return Err(format!(
            "device instance changed since approval: restore job approved {:?} but the \
             present {:04x}:{:04x} is {:?} — refusing",
            job.instance_id, job.vid, job.pid, device.instance_id
        ));
    }
    transaction_log(
        "restore-driver",
        &format!("restoring in-box driver: instance={}", device.instance_id),
    );
    aokie_dongle::winusb::restore_inbox_driver(job.vid, job.pid)?;
    log_line!(
        "[aokie-driver-helper] restored in-box driver for {}",
        device.instance_id
    );
    Ok(())
}

/// AOK-DRIVER-001/DRIVER-001: the live device instance must match the one
/// the dispatcher approved — exact, case-insensitive, never skipped (an
/// empty approved id is refused earlier by the mandatory-field checks).
fn instance_matches(approved_instance: &str, live_instance: &str) -> bool {
    !approved_instance.is_empty() && approved_instance.eq_ignore_ascii_case(live_instance)
}

/// AOK-DRIVER-001/DRIVER-001: the INF on disk must hash to the value the
/// dispatcher approved — never skipped.
fn inf_hash_matches(approved_hash: &str, actual_hash: &str) -> bool {
    !approved_hash.is_empty() && approved_hash.eq_ignore_ascii_case(actual_hash)
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
    let name = inf_path.file_name().and_then(|name| name.to_str());
    if !name.is_some_and(|name| name.eq_ignore_ascii_case(aokie_dongle::winusb::INF_NAME)) {
        return Err(format!(
            "inf_path must name the pinned package file {}, got {:?}",
            aokie_dongle::winusb::INF_NAME,
            name
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

    #[test]
    fn release_self_sign_capability_requires_managed_beta_feature() {
        assert!(self_sign_capable(true, false));
        assert!(!self_sign_capable(false, false));
        assert!(self_sign_capable(false, true));
    }

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
        let inf = dir_inf.join(aokie_dongle::winusb::INF_NAME);
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
        let inf = dir.join(aokie_dongle::winusb::INF_NAME);
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
    fn empty_approved_instance_is_refused() {
        // DRIVER-001: the legacy "empty field skips the check" tolerance is
        // gone — an unpinned job can never match a live device.
        assert!(!instance_matches("", "USB\\VID_0A5C&PID_21EC\\ANY"));
    }

    #[test]
    fn inf_hash_match_is_case_insensitive_and_never_skipped() {
        let h = "ABCD1234";
        assert!(inf_hash_matches(h, "abcd1234"));
        assert!(!inf_hash_matches(h, "0000ffff"));
        // DRIVER-001: an empty approved hash no longer passes anything.
        assert!(!inf_hash_matches("", "anything"));
    }

    fn v5_job(instance_id: &str, inf_sha256: &str, hardware_id: &str) -> DriverJob {
        serde_json::from_str(&format!(
            r#"{{"version":5,"mode":"install","vid":2652,"pid":8684,
                "inf_path":"C:\\x\\aokie_winusb_bluetooth.inf",
                "instance_id":{:?},"inf_sha256":{:?},"cat_sha256":{:?},"hardware_id":{:?}}}"#,
            instance_id, inf_sha256, GOOD_SHA, hardware_id
        ))
        .unwrap()
    }

    const GOOD_SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn v5_install_job_requires_every_exact_target_field() {
        // DRIVER-001 acceptance: missing fields abort before mutation.
        let hwid = aokie_dongle::winusb::hardware_id(2652, 8684);
        let good = v5_job("USB\\VID_0A5C&PID_21EC\\00198600226C", GOOD_SHA, &hwid);
        validate_install_job_fields(&good).unwrap();
        assert!(!good.allow_dev_self_sign, "self-signing defaults OFF");

        let mut managed = v5_job("USB\\VID_0A5C&PID_21EC\\00198600226C", GOOD_SHA, &hwid);
        managed.allow_dev_self_sign = true;
        managed.cat_sha256.clear();
        validate_install_job_fields(&managed).unwrap();

        let err = validate_install_job_fields(&v5_job("", GOOD_SHA, &hwid)).unwrap_err();
        assert!(err.contains("no device instance id"), "got: {}", err);

        let err = validate_install_job_fields(&v5_job("USB\\X\\1", "deadbeef", &hwid)).unwrap_err();
        assert!(err.contains("not a 64-hex SHA-256"), "got: {}", err);

        let err = validate_install_job_fields(&v5_job("USB\\X\\1", GOOD_SHA, "")).unwrap_err();
        assert!(
            err.contains("does not match the id derived"),
            "got: {}",
            err
        );

        let err =
            validate_install_job_fields(&v5_job("USB\\X\\1", GOOD_SHA, "USB\\VID_1234&PID_5678"))
                .unwrap_err();
        assert!(
            err.contains("does not match the id derived"),
            "got: {}",
            err
        );
    }

    #[test]
    fn stale_v3_job_is_rejected_by_the_version_gate() {
        // Serde still parses older versions (so the error is a clean version
        // message, not a parse failure) — but run() refuses anything != v4.
        let json = r#"{"version":3,"mode":"install","vid":2652,"pid":8684,
                       "inf_path":"C:\\x\\aokie_winusb_bluetooth.inf",
                       "instance_id":"USB\\VID_0A5C&PID_21EC\\00198600226C",
                       "inf_sha256":"deadbeef"}"#;
        let job: DriverJob = serde_json::from_str(json).unwrap();
        assert_eq!(job.version, 3);
        assert_ne!(job.version, SUPPORTED_JOB_VERSION);
    }

    #[test]
    fn v5_job_round_trips_the_new_fields() {
        let json = r#"{"version":5,"mode":"install","vid":2652,"pid":8684,
                       "inf_path":"C:\\x\\aokie_winusb_bluetooth.inf",
                       "instance_id":"USB\\VID_0A5C&PID_21EC\\00198600226C",
                       "inf_sha256":"deadbeef",
                       "cat_sha256":"feedface",
                       "hardware_id":"USB\\VID_0A5C&PID_21EC",
                       "allow_dev_self_sign":true}"#;
        let job: DriverJob = serde_json::from_str(json).unwrap();
        assert_eq!(job.version, 5);
        assert_eq!(job.cat_sha256, "feedface");
        assert_eq!(job.hardware_id, "USB\\VID_0A5C&PID_21EC");
        assert!(job.allow_dev_self_sign);
    }
}
