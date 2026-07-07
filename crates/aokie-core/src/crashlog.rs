//! Crash diagnostics — panic hook + stderr redirect.
//!
//! Aokie ships as a Windows GUI app with no console, so anything
//! printed to stderr (Rust `eprintln!`, ORT's C++ logger, CUDA driver
//! errors) is invisible by default. When a model load crashes silently
//! the operator has nothing to send back. This module fixes both ends:
//!
//! 1. **Panic hook** (`install_panic_hook`) — captures Rust panics with
//!    a backtrace into `<app_data>/aokie-crash.log`. Catches any panic
//!    raised after `install()` runs.
//! 2. **stdout / stderr redirect** (`redirect_std_streams` on Windows)
//!    — rebinds `STD_OUTPUT_HANDLE` / `STD_ERROR_HANDLE` and the C
//!    runtime's `stdout` / `stderr` `FILE*` to `<app_data>/aokie-log.log`.
//!    Catches output from Rust `println!`/`eprintln!`, ORT's logger,
//!    and any C++ library that writes to `printf` / `fprintf(stderr,…)`.
//!    Both streams share one file so a crash log reads in chronological
//!    order regardless of which stream printed which line.
//!
//! ## Rotation
//!
//! Both files are size-rotated before they're (re)opened: when the
//! file exceeds `LOG_ROTATE_BYTES` we shift `aokie-log.log → .log.1 →
//! .log.2 → …` up to `LOG_ROTATE_KEEP` archives, then drop the oldest.
//! Without this a long-running install accumulates an arbitrarily
//! large log — verbose ORT logging plus PII-redacted call transcripts
//! grow into hundreds of MB on a busy receptionist over a few weeks.
//! The default of 5 × 10 MB caps the on-disk footprint at ~50 MB
//! while still preserving enough history for a typical incident
//! report.
//!
//! Call `install()` once, as early in `run()` as possible — before
//! any code that might log or panic. Idempotent (the second call is a
//! no-op via the static atomic).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Roll the log file when it crosses this size before reopening.
/// 10 MB per file is wide enough that a single panic dump fits
/// without splitting and small enough that grepping with notepad is
/// still practical.
const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Number of archive copies to keep alongside the live file. With
/// `LOG_ROTATE_BYTES = 10 MB` and `LOG_ROTATE_KEEP = 5`, the on-disk
/// footprint caps at roughly 60 MB: one live file + five archives.
const LOG_ROTATE_KEEP: usize = 5;

/// Install the panic hook + stderr redirect. Must be called early in
/// `run()` so it covers as much of the program as possible. Idempotent.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    // RUST_BACKTRACE=1 turns on full backtraces in panic messages —
    // without this, the crash log only shows the panic message and
    // location, not the call stack. Skip if the user already set a
    // value (so `RUST_BACKTRACE=full` keeps its richer output).
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    // Resolve the app_data dir without an AppHandle — Tauri 2's
    // `app_data_dir()` is `<RoamingAppData>/<bundle_identifier>`, so
    // `dirs::data_dir().join(<id>)` produces the same path. We can't
    // use AppHandle here because the panic hook needs to be live before
    // the Tauri builder runs (so a panic during builder setup is also
    // caught). Use `crate::paths::BUNDLE_IDENTIFIER` (build-time bridge
    // from `tauri.conf.json`, see R4-#15) so a future identifier rename
    // doesn't leave install() writing to one path while
    // wipe_logs_best_effort() reads from another.
    let Some(data_dir) = dirs::data_dir() else {
        return;
    };
    let app_data = data_dir.join(crate::paths::BUNDLE_IDENTIFIER);
    if std::fs::create_dir_all(&app_data).is_err() {
        return;
    }

    // Rotate before opening either file — we'd rather drop the oldest
    // archive than truncate the live file the operator is about to
    // append to. Best-effort: rotation failures (locked file, perms)
    // fall through and the log opens at its current size.
    rotate_if_needed(&app_data.join("aokie-crash.log"));
    #[cfg(target_os = "windows")]
    rotate_if_needed(&app_data.join("aokie-log.log"));

    install_panic_hook(&app_data);

    #[cfg(target_os = "windows")]
    redirect_std_streams(&app_data);
}

/// R4-#11: best-effort sweep of the panic / stderr-redirect log
/// files this module owns. Called by `delete_all_local_data_now`
/// when the operator wants to wipe every local artefact (shared /
/// stolen / sold machine recovery story). Returns the count of
/// files actually removed; failures are silent — a log file held
/// open by the redirect path on Windows can't be removed mid-run,
/// but the next launch's rotation will overwrite it anyway, and
/// the operator's intent ("stop holding any of my data") is met
/// for everything else that did delete.
pub fn wipe_logs_best_effort() -> usize {
    let Some(data_dir) = dirs::data_dir() else {
        return 0;
    };
    let app_data = data_dir.join(crate::paths::BUNDLE_IDENTIFIER);
    if !app_data.exists() {
        return 0;
    }
    let mut removed = 0usize;
    // Live log files this module writes to.
    for name in ["aokie-crash.log", "aokie-log.log"] {
        let live = app_data.join(name);
        if live.exists() && std::fs::remove_file(&live).is_ok() {
            removed += 1;
        }
        // Rotation archives: name.log.1 .. name.log.<LOG_ROTATE_KEEP>.
        for n in 1..=LOG_ROTATE_KEEP {
            let mut path_os = live.as_os_str().to_owned();
            path_os.push(format!(".{}", n));
            let archive = PathBuf::from(path_os);
            if archive.exists() && std::fs::remove_file(&archive).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// If `path` is at or past the rotation threshold, roll it: the live
/// file becomes `.log.1`, `.log.1` slides to `.log.2`, and so on
/// until `LOG_ROTATE_KEEP` archives are filled — the oldest one is
/// deleted to make room. Best-effort: any I/O error along the way
/// is swallowed (rotation is a hygiene step, not a load-bearing
/// one) and the existing file is left in place.
///
/// Naming preserves the original suffix instead of stripping it
/// (`aokie-crash.log` → `aokie-crash.log.1`), so a Windows search
/// for `*.log` still surfaces archives.
fn rotate_if_needed(path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return; // file doesn't exist yet — first run, nothing to rotate
    };
    if metadata.len() < LOG_ROTATE_BYTES {
        return;
    }
    // Shift the highest-numbered archive out, then walk down. The
    // oldest one (`.log.<KEEP>`) is dropped; intermediate ones slide
    // one slot up.
    let archive = |n: usize| -> PathBuf {
        let mut p = path.as_os_str().to_owned();
        p.push(format!(".{}", n));
        PathBuf::from(p)
    };
    let oldest = archive(LOG_ROTATE_KEEP);
    let _ = std::fs::remove_file(&oldest);
    for n in (1..LOG_ROTATE_KEEP).rev() {
        let from = archive(n);
        let to = archive(n + 1);
        if from.exists() {
            let _ = std::fs::rename(&from, &to);
        }
    }
    // Live file → `.log.1`. If the rename fails (e.g. the redirect
    // already opened it on Windows), leave the file alone — we'll
    // catch it on the next launch when nothing has it open.
    let _ = std::fs::rename(path, archive(1));
}

fn install_panic_hook(app_data: &Path) {
    let crash_log_path = app_data.join("aokie-crash.log");
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort write to the crash log. Failures (disk full,
        // permission denied) are swallowed — there's nothing useful
        // we can do from inside a panic hook anyway, and the prev_hook
        // call below still prints to stderr.
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&crash_log_path)
        {
            let timestamp = chrono::Utc::now().to_rfc3339();
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<unknown>".to_string());
            let payload = info.payload();
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };
            let backtrace = std::backtrace::Backtrace::force_capture();
            let _ = writeln!(file, "\n=== PANIC {} at {} ===", timestamp, location);
            let _ = writeln!(file, "{}", msg);
            let _ = writeln!(file, "{}", backtrace);
            let _ = writeln!(file, "=== end panic ===\n");
            let _ = file.flush();
        }
        // Chain to the previous (default) hook so the panic still goes
        // to stderr — which is now being captured to aokie-stderr.log
        // anyway, plus any console attached during dev.
        prev_hook(info);
    }));
}

#[cfg(target_os = "windows")]
fn redirect_std_streams(app_data: &Path) {
    // Two-step redirect, applied to both stdout and stderr. Win32's
    // `STD_OUTPUT_HANDLE` / `STD_ERROR_HANDLE` covers what Rust's
    // stdlib writes (Rust resolves these via `GetStdHandle`); the C
    // runtime's `stdout` / `stderr` `FILE*` covers what C/C++ libraries
    // (ORT, CUDA) write via `printf`/`fprintf`. They share an
    // underlying file descriptor on most setups but rebinding both is
    // the only way to be sure both Rust and native output end up in
    // the file.
    //
    // Both streams point at the same file so the operator can read a
    // single chronological log instead of stitching two together.
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, SetFilePointer, FILE_ATTRIBUTE_NORMAL, FILE_END, FILE_GENERIC_WRITE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
    };
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

    let log_path = app_data.join("aokie-log.log");
    let path_w: Vec<u16> = log_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `path_w` is null-terminated and lives for the duration
    // of the call. CreateFileW returns INVALID_HANDLE_VALUE on failure;
    // we check for that. The handle is intentionally leaked into the
    // process's STD_*_HANDLE — Windows reclaims it at process exit.
    unsafe {
        let h: HANDLE = CreateFileW(
            path_w.as_ptr(),
            FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        );
        // CreateFileW returns INVALID_HANDLE_VALUE on failure. HANDLE
        // in windows-sys 0.59 is an `isize`, so the comparison (not a
        // null check) is the right gate.
        if h == INVALID_HANDLE_VALUE {
            return;
        }
        // Seek to end so we append rather than truncate. Return value
        // is the new file position; we don't need it.
        let _ = SetFilePointer(h, 0, ptr::null_mut(), FILE_END);

        // Rust println!/eprintln!/panic-hook output → this file via
        // the std-handle table.
        SetStdHandle(STD_OUTPUT_HANDLE, h);
        SetStdHandle(STD_ERROR_HANDLE, h);

        // C runtime's stdout/stderr FILE* → same file via _wfreopen.
        // Without this, ORT's C++ logger keeps writing to the original
        // (now detached) streams and we lose its diagnostics. UCRT
        // exposes the standard streams via `__acrt_iob_func(idx)`
        // (idx: 0=stdin, 1=stdout, 2=stderr); the wide-char freopen
        // rebinds the FILE* in place.
        extern "C" {
            fn _wfreopen(
                path: *const u16,
                mode: *const u16,
                stream: *mut core::ffi::c_void,
            ) -> *mut core::ffi::c_void;
            fn __acrt_iob_func(idx: u32) -> *mut core::ffi::c_void;
        }
        let mode_w: Vec<u16> = "a\0".encode_utf16().collect();
        for idx in [1u32, 2u32] {
            let f = __acrt_iob_func(idx);
            if !f.is_null() {
                let _ = _wfreopen(path_w.as_ptr(), mode_w.as_ptr(), f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    /// Per-test temp dir without pulling in the `tempfile` dev-dep —
    /// uuid is already a workspace dep and gives us a unique prefix.
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let p = std::env::temp_dir()
                .join(format!("aokie_crashlog_rotate_{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Write a file padded to `size` bytes so the rotation gate fires.
    fn write_n(path: &Path, size: u64, marker: &str) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .unwrap();
        f.write_all(marker.as_bytes()).unwrap();
        // Zero-pad to push the file across the threshold.
        let pad = size.saturating_sub(marker.len() as u64);
        f.set_len(marker.len() as u64 + pad).unwrap();
    }

    #[test]
    fn rotate_below_threshold_is_a_noop() {
        let tmp = TestDir::new();
        let log = tmp.path().join("aokie-log.log");
        write_n(&log, 1024, "live");
        rotate_if_needed(&log);
        // Live file untouched. No archive should appear because we
        // never crossed the threshold.
        assert!(log.exists());
        assert!(!tmp.path().join("aokie-log.log.1").exists());
    }

    #[test]
    fn rotate_at_threshold_creates_first_archive() {
        let tmp = TestDir::new();
        let log = tmp.path().join("aokie-log.log");
        write_n(&log, LOG_ROTATE_BYTES, "live");
        rotate_if_needed(&log);
        // Live file got pushed into `.log.1`; nothing fresh appeared
        // at `aokie-log.log` (the install path opens it on the next
        // call).
        assert!(!log.exists(), "live file moved to archive");
        let arc1 = tmp.path().join("aokie-log.log.1");
        assert!(arc1.exists(), "archive .1 created from live file");
        assert!(arc1.metadata().unwrap().len() >= LOG_ROTATE_BYTES);
    }

    #[test]
    fn rotate_drops_oldest_when_keep_full() {
        let tmp = TestDir::new();
        let log = tmp.path().join("aokie-log.log");
        // Pre-seed the slot stack: live + KEEP archives, all big.
        write_n(&log, LOG_ROTATE_BYTES, "live");
        for n in 1..=LOG_ROTATE_KEEP {
            let mut p = log.as_os_str().to_owned();
            p.push(format!(".{}", n));
            write_n(&PathBuf::from(p), 256, &format!("archive-{}", n));
        }
        // Trigger rotation. The oldest (`.log.<KEEP>`) should be
        // dropped, every other archive shifts up by one, and the
        // live file becomes `.log.1`.
        rotate_if_needed(&log);
        assert!(!log.exists());
        let arc1 = tmp.path().join("aokie-log.log.1");
        assert_eq!(
            String::from_utf8_lossy(&fs::read(&arc1).unwrap()).trim_matches(char::from(0)),
            "live"
        );
        // .log.2 holds the previous .log.1, etc., up to .log.<KEEP>.
        for n in 2..=LOG_ROTATE_KEEP {
            let mut p = log.as_os_str().to_owned();
            p.push(format!(".{}", n));
            let body = fs::read(&PathBuf::from(p)).unwrap();
            let s = String::from_utf8_lossy(&body);
            assert!(
                s.starts_with(&format!("archive-{}", n - 1)),
                ".log.{} should hold the prior .log.{}, got {:?}",
                n,
                n - 1,
                s.chars().take(16).collect::<String>()
            );
        }
        // No `.log.<KEEP+1>` ever appears — the rotation cap holds.
        let mut overflow = log.as_os_str().to_owned();
        overflow.push(format!(".{}", LOG_ROTATE_KEEP + 1));
        assert!(
            !PathBuf::from(overflow).exists(),
            "rotation must not exceed LOG_ROTATE_KEEP archives"
        );
    }

    #[test]
    fn rotate_missing_file_is_silent() {
        let tmp = TestDir::new();
        // No file present — rotation should be a no-op (covers the
        // first-launch path before any log has been written).
        rotate_if_needed(&tmp.path().join("aokie-log.log"));
        // No directory entries created.
        let entries: Vec<_> = fs::read_dir(tmp.path()).unwrap().collect();
        assert!(
            entries.is_empty(),
            "first-launch rotation must not create files"
        );
    }
}
