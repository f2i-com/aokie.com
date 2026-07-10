//! Build provenance (audit CROSS-OBS-001): stamp the git ref into the binary
//! so `plugin.health` / support bundles can say exactly WHICH build answered.
//! Best-effort — a non-git build (source tarball) stamps "unknown" rather
//! than failing.

fn main() {
    let git_ref = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=AOKIE_BUILD_REF={git_ref}");
    // Re-stamp when HEAD moves (commit/checkout), not on every build.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
