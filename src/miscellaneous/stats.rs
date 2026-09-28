//! Code statistics: the report types and the tokei computation live in
//! `clew_core::stats` (shared with clew-server, which answers the `Stats`
//! request for remote projects); this module keeps the client-side cache.
//!
//! Like `overview`, the report is a derived artifact cached in the project's
//! derived-artifact directory (`clew_core::derived::dir`, in clew's own data
//! directory), keyed by the change-detection registry's revision, so it is
//! served instantly on reopen and recomputed only after files changed.

use std::path::{Path, PathBuf};

pub use clew_core::stats::{LangStat, StatsReport, compute};

/// A persisted report plus the registry revision it was computed at, so a
/// changed file set (or edited file) misses the cache.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Cached {
    pub report: StatsReport,
    pub rev: u64,
}

fn cache_path(store: &Path) -> PathBuf {
    store.join("stats.json")
}

/// Load the persisted stats (None on any error / not yet computed).
pub fn load(store: &Path) -> Option<Cached> {
    clew_core::statefile::read(&cache_path(store)).and_then(|s| serde_json::from_str(&s).ok())
}

/// Persist the stats (atomic temp+rename).
pub fn save(store: &Path, cached: &Cached) -> std::io::Result<()> {
    let json = serde_json::to_string(cached).map_err(|e| std::io::Error::other(e.to_string()))?;
    clew_core::statefile::write_atomic(&cache_path(store), json.as_bytes())
}
