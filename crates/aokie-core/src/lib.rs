//! # aokie-core
//!
//! Tauri-free native foundation shared by the other Aokie crates
//! (`aokie-bluetooth`, `aokie-dongle`, `aokie-audio`, `aokie-db`) and,
//! later, the FormLogic Desktop plugin (`crates/aokie-plugin`).
//!
//! These modules were lifted verbatim out of the legacy Aokie Tauri
//! app (`aokie-desktop/src-tauri/src`); the only edits made while
//! moving them were to drop Tauri command wrappers and re-point a few
//! cross-module paths at the new crate boundaries.
//!
//! Contract rule: **no Tauri imports allowed in this crate** —
//! enforced by the `no_tauri` test module at the bottom of this file,
//! which fails if Tauri shows up as a `Cargo.toml` dependency or as a
//! code-level path/attribute reference in any source file. (Prose
//! mentions of Tauri are fine; the guard matches the code tokens only.)

pub mod consent;
pub mod crashlog;
pub mod dongle_catalog;
pub mod events;
pub mod model_verify_cache;
pub mod paths;
pub mod redact;
pub mod retention;
pub mod secrets;
pub mod speech;
pub mod url_classification;

/// CI-style guard: this crate must stay tauri-free so downstream
/// consumers (the plugin process, the native crates) never link the
/// webview stack. Greps the crate's own manifest and sources rather
/// than the dep graph — a Tauri dependency cannot exist without
/// appearing in `Cargo.toml`, and a Tauri path/attribute token cannot
/// compile without a dependency.
#[cfg(test)]
mod no_tauri {
    use std::path::Path;

    fn source_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read_dir src") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                source_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn manifest_has_no_tauri_dependency() {
        let manifest = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        )
        .expect("read Cargo.toml");
        // Any dependency on tauri (or a tauri-* plugin/build crate)
        // must name it in the manifest; the description mentioning the
        // word is fine, a `tauri` dep key or "tauri" package name is not.
        for line in manifest.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            assert!(
                !(line.starts_with("tauri") || line.contains("\"tauri")),
                "aokie-core must not depend on tauri; offending Cargo.toml line: {line}"
            );
        }
    }

    #[test]
    fn sources_have_no_tauri_references() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        source_files(&src, &mut files);
        assert!(!files.is_empty(), "no sources found under {}", src.display());
        // Needles are assembled at runtime so this test file doesn't
        // trip over its own literals.
        let path_ref = ["tauri", "::"].concat();
        let attr_ref = ["#[", "tauri"].concat();
        for file in files {
            let text = std::fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
            for (n, line) in text.lines().enumerate() {
                // Prose may say "Tauri"; code must never path into it.
                assert!(
                    !line.contains(&path_ref) && !line.contains(&attr_ref),
                    "tauri reference in {}:{}: {}",
                    file.display(),
                    n + 1,
                    line.trim()
                );
            }
        }
    }
}
