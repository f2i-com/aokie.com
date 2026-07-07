//! Single helper module for resolving Aokie's on-disk paths.
//!
//! Two roots are in play across the codebase:
//!
//!   * **Tauri app-data dir** (`<dirs::data_dir>/com.aokie.app`): the
//!     canonical location returned by `app.path().app_data_dir()` when
//!     a Tauri command has access to the `AppHandle`. Per-bundle
//!     directory based on the `identifier` in `tauri.conf.json`.
//!     Used for: AI provider override, VAD models, pairing store.
//!
//!   * **Legacy aokie dir** (`<dirs::config_dir>/aokie`): older
//!     pre-bundle-identifier path used by the original Phase 4
//!     `save_config_to_file` command. Still holds `config.json` on
//!     existing installs, so reads check this location first.
//!
//! Background tasks (the SMS auto-reply pipeline, post-call extraction)
//! often run without an `AppHandle` in scope. They go through the
//! AppHandle-free helpers here, which mirror what
//! `app.path().app_data_dir()` would have returned. The
//! AppHandle-aware variants exist for callers that already have one
//! and want to surface a specific Tauri error type.
//!
//! When you add a new on-disk file, prefer `app_data_dir()` —
//! per-bundle paths are forwards-compatible with future bundle id
//! changes (a `com.aokie.app` → `com.aokie.desktop` rename would
//! split user data along the new identifier without polluting the
//! shared `aokie/` directory).

use std::path::{Path, PathBuf};

/// Bundle identifier mirroring `tauri.conf.json::identifier`. Kept
/// as a compile-time constant so background tasks (auto-reply,
/// post-call extraction) can reach the per-bundle data dir without
/// an `AppHandle`.
///
/// R4-#15: resolved at build time from `tauri.conf.json` via
/// `build.rs` — the env var is always set (build.rs falls back to
/// `com.aokie.app` if the JSON is unreadable). Renaming the Tauri
/// identifier now flows through to this constant automatically;
/// without the build bridge a rename silently left the Rust
/// path-resolution code looking under the old app-data dir.
pub const BUNDLE_IDENTIFIER: &str = env!("AOKIE_BUNDLE_IDENTIFIER");

/// `<dirs::data_dir>/com.aokie.app` — the canonical Tauri app-data
/// dir. Returns `None` only on platforms where `dirs::data_dir` itself
/// fails (no $HOME, no $XDG_DATA_HOME, etc.).
pub fn app_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|p| p.join(BUNDLE_IDENTIFIER))
}

/// `<dirs::config_dir>/aokie` — pre-bundle-identifier legacy path that
/// still holds `config.json` for existing installs. Prefer
/// `app_data_dir()` for new files.
pub fn legacy_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("aokie"))
}

/// Path to the persisted `config.json` at the legacy location. Pre
/// 2026-05 installs wrote here; reads still consult it as a fallback
/// until the next save migrates the file.
pub fn legacy_config_json() -> Option<PathBuf> {
    legacy_config_dir().map(|d| d.join("config.json"))
}

/// Path to the canonical `config.json` under the Tauri app-data dir.
/// All new writes target this path; the legacy location is read-only
/// fallback. Returns `None` only when `dirs::data_dir` itself fails.
pub fn canonical_config_json() -> Option<PathBuf> {
    app_data_dir().map(|d| d.join("config.json"))
}

/// Resolve the path readers should hit for `config.json`. Prefers the
/// canonical app-data location; falls back to legacy when the
/// canonical file isn't there yet (fresh upgrades from a pre-migration
/// install). Returns `None` only when both lookups fail.
///
/// Only consumed by the Windows BT call body in
/// `bluetooth_commands.rs`; on Linux the developer-preview build
/// doesn't load `config.json` here (frontend Pairing UX is L5c, see
/// PLAN.md), so cargo flags it as dead. Suppress on Linux.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn resolve_config_json_for_read() -> Option<PathBuf> {
    if let Some(canonical) = canonical_config_json() {
        if canonical.exists() {
            return Some(canonical);
        }
    }
    let legacy = legacy_config_json()?;
    if legacy.exists() {
        return Some(legacy);
    }
    canonical_config_json()
}

/// Copy `<legacy>/config.json` into `<canonical>/config.json` if the
/// canonical file is missing. Runs once at startup so subsequent reads
/// can target the canonical path. Best-effort — failures are logged
/// and the legacy file stays in place for the next launch to retry.
pub fn migrate_legacy_config_if_needed() {
    let (Some(legacy), Some(canonical)) = (legacy_config_json(), canonical_config_json()) else {
        return;
    };
    if canonical.exists() {
        return;
    }
    if !legacy.exists() {
        return;
    }
    if let Some(parent) = canonical.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!(
                "[paths] migrate config: mkdir {:?} failed: {} — keeping legacy",
                parent, e
            );
            return;
        }
    }
    match std::fs::read(&legacy) {
        Ok(bytes) => {
            if let Err(e) = atomic_write(&canonical, &bytes) {
                eprintln!(
                    "[paths] migrate config: write {:?} failed: {} — keeping legacy",
                    canonical, e
                );
                return;
            }
        }
        Err(e) => {
            eprintln!(
                "[paths] migrate config: read {:?} failed: {} — keeping legacy",
                legacy, e
            );
            return;
        }
    }
    println!(
        "[paths] Migrated {:?} -> {:?} (legacy kept as backup; future writes go to canonical)",
        legacy, canonical
    );
}

/// Atomic-write helper: writes `bytes` to a sibling `<path>.tmp`, fsyncs
/// it, then renames onto `path`. Same-filesystem rename is atomic on
/// every OS we ship to, so a crash or power-loss leaves us with either
/// the old contents or the new — never a half-written / truncated file.
///
/// Without this, `std::fs::write` truncates the destination first then
/// writes, so an interrupt between truncate and write produces an empty
/// file and the next launch silently boots with defaults — easy way to
/// nuke user settings or an AI provider override.
///
/// Concurrency: each call mints a unique tmp suffix (PID + monotonic
/// counter) so two writers landing on the same destination don't fight
/// over a shared `.tmp` filename. On Unix the file lands on disk via
/// `O_CREAT|O_TRUNC|O_WRONLY` with mode 0600, then `fsync(file)`
/// before rename, then `fsync(parent_dir)` after rename — that final
/// directory fsync is what makes the rename itself power-loss durable
/// on ext4/XFS/btrfs/APFS. Windows doesn't expose a directory-fsync
/// equivalent and `MoveFileEx`/`rename` is atomic on NTFS for
/// same-volume moves anyway, so we skip the parent fsync there.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {:?}: {}", parent, e))?;
    }

    // Unique tmp filename: `<file>.<ext>.<pid>-<seq>.tmp`. The previous
    // shape (`<file>.<ext>.tmp`) collided when two callers raced on
    // the same destination — one would see ENOENT during rename
    // because the other had already moved the shared tmp away.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_suffix = format!("{}-{}.tmp", std::process::id(), seq);
    let tmp_path = path.with_extension(match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{}.{}", ext, tmp_suffix),
        None => tmp_suffix,
    });

    {
        // Everything we write under app_data_dir is per-user state, and
        // some of it is outright credential material (pairing-store link
        // keys, provider config files holding keyring secret_refs). On
        // Unix the home directory is world-traversable by default, so
        // create the tmp file with 0600 from the get-go — chmod after
        // create would leave a brief window where another local user
        // could open the file before we lock it down. On Windows the
        // file inherits the ACL of `%APPDATA%\com.aokie.app\`, which is
        // user-restricted by NTFS by default, so default permissions
        // are safe.
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)
                .map_err(|e| format!("create {:?}: {}", tmp_path, e))?
        };
        #[cfg(not(unix))]
        let mut f = std::fs::File::create(&tmp_path)
            .map_err(|e| format!("create {:?}: {}", tmp_path, e))?;
        f.write_all(bytes)
            .map_err(|e| format!("write {:?}: {}", tmp_path, e))?;
        f.sync_all()
            .map_err(|e| format!("fsync {:?}: {}", tmp_path, e))?;
    }
    std::fs::rename(&tmp_path, path)
        .map_err(|e| format!("rename {:?} -> {:?}: {}", tmp_path, path, e))?;

    // Power-loss durability for the rename itself: on POSIX the
    // directory entry lives in the parent inode, so the rename hasn't
    // hit non-volatile storage until the parent dir is fsynced. Best-
    // effort — a failure here means the rename is in OS cache but not
    // on the platter, so the previous file content might come back
    // after a crash. Surface the error so the caller can choose to
    // retry, but don't undo the rename (the tmp is gone now).
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::File::open(parent).and_then(|f| f.sync_all()) {
            return Err(format!("fsync parent dir {:?}: {}", parent, e));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_data_dir_ends_with_bundle_identifier() {
        let dir = app_data_dir().expect("dirs::data_dir resolves on test host");
        assert_eq!(
            dir.file_name().and_then(|n| n.to_str()),
            Some(BUNDLE_IDENTIFIER)
        );
    }

    #[test]
    fn legacy_config_dir_ends_with_aokie() {
        let dir = legacy_config_dir().expect("dirs::config_dir resolves on test host");
        assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("aokie"));
    }

    #[test]
    fn atomic_write_replaces_existing_contents() {
        let tmp =
            std::env::temp_dir().join(format!("aokie-atomic-write-{}.json", std::process::id()));
        std::fs::write(&tmp, b"old").unwrap();
        atomic_write(&tmp, b"{\"new\":true}").unwrap();
        let read = std::fs::read_to_string(&tmp).unwrap();
        assert_eq!(read, "{\"new\":true}");
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn atomic_write_creates_parent_dir() {
        let root =
            std::env::temp_dir().join(format!("aokie-atomic-write-mkdir-{}", std::process::id()));
        let path = root.join("nested/sub/config.json");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        std::fs::remove_dir_all(&root).ok();
    }

    /// The tmp file we write to must not collide with the destination
    /// path — otherwise a concurrent reader could observe the partial
    /// write. After a successful atomic_write the staging file should
    /// be gone (renamed atop the destination).
    #[test]
    fn atomic_write_uses_distinct_tmp_path() {
        let root =
            std::env::temp_dir().join(format!("aokie-atomic-write-tmp-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.json");
        atomic_write(&path, b"x").unwrap();
        assert!(path.exists());
        // Walk the directory to make sure no `.tmp` siblings linger,
        // regardless of the exact tmp filename shape.
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|n| n.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "stale tmp files: {:?}", leftovers);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Concurrent writers on the same destination shouldn't race over
    /// a shared tmp filename. Each call mints its own
    /// `<pid>-<seq>.tmp` so both writes succeed; one of them wins
    /// the rename last and is what the file ends up holding.
    #[test]
    fn atomic_write_handles_concurrent_writers() {
        let root =
            std::env::temp_dir().join(format!("aokie-atomic-write-race-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.json");

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let p = path.clone();
                std::thread::spawn(move || {
                    atomic_write(&p, format!("payload-{}", i).as_bytes()).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let final_contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            final_contents.starts_with("payload-"),
            "got: {:?}",
            final_contents
        );

        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|n| n.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "stale tmp files after race: {:?}",
            leftovers
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
