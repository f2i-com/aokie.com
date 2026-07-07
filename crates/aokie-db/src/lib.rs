//! # aokie-db
//!
//! Aokie's SQLite layer, lifted verbatim out of the legacy Tauri app
//! (`aokie-desktop/src-tauri/src/database`): the migration ladder, the
//! call-log / transcript / contacts / SMS schema, the FTS5 free-text
//! search index, and the retention-purge + delete-now operations.
//!
//! The legacy code threaded a `tauri::AppHandle` into every entry point
//! to resolve the app-data dir and surface db errors. That single Tauri
//! coupling has been replaced by the [`DbHost`] trait, which the caller
//! implements — so this crate has **zero** Tauri dependency. A stock
//! host is a one-liner over [`aokie_core::paths::app_data_dir`].

pub mod database;

pub use database::DbHost;
