//! Persistent warm-start cache for the symbol index and content hashes, stored
//! under `<root>/.clew/cache/`.
//!
//! On reopen, most files have not changed while clew was closed. Rather than
//! re-read and re-parse the whole tree, this cache lets the index build confirm
//! each file cheaply and reuse its cached symbols + hash when the content is
//! unchanged, re-parsing only what actually changed.
//!
//! Correctness rules (this is a pure accelerator — it must never yield a wrong
//! result):
//!   * A file's cached symbols/hash are reused **only after the content is
//!     confirmed unchanged**: a `stat()` fast path (mtime + size both match)
//!     or, when that differs, a content-hash match. The mtime fast path is the
//!     industry-standard heuristic (git, make, rust-analyzer); its only hole —
//!     identical mtime *and* size but different bytes — is not reachable by any
//!     normal edit and self-heals on the next real change (the live watcher
//!     re-hashes it).
//!   * The file carries a schema `version`; a mismatch (e.g. after a clew
//!     upgrade that changes symbol extraction) makes the whole cache be ignored
//!     and rebuilt. **Bump [`CACHE_VERSION`] whenever symbol extraction or the
//!     hash changes.**
//!   * Any read/parse/version error falls back to an empty cache (full rebuild).
//!   * `.clew/` is git-ignored, so the cache is strictly local and never shared.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::incremental::Version;

/// Bump on any change to symbol/import extraction or the content hash so stale
/// caches from an older clew are discarded rather than trusted.
const CACHE_VERSION: u32 = 3;

/// A cached symbol (the index entry minus the paths, which are reconstructed
/// from the project root + relative path on load).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSymbol {
    pub name: String,
    pub kind: String,
    pub line: usize,
    /// Whether this is a test function (a `#[…test…]` attribute or test-name
    /// convention). Cached so the outline/call-graph don't re-scan per frame.
    #[serde(default)]
    pub is_test: bool,
}

/// A cached raw import (the extraction result, resolved lazily against the live
/// file set — resolution is cheap and depends on files that may have moved).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedImport {
    pub module: String,
    pub line: usize,
    #[serde(default)]
    pub is_mod: bool,
}

/// Everything cached about one file: how to confirm it is unchanged (mtime,
/// size, content hash) and the derived artifacts to reuse when it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCache {
    pub mtime_ns: u64,
    pub size: u64,
    pub hash: Version,
    pub symbols: Vec<CachedSymbol>,
    /// Raw imports (only the definition text is extracted; resolution is not
    /// cached). Defaulted so a partial/older entry still loads.
    #[serde(default)]
    pub imports: Vec<CachedImport>,
}

/// The on-disk, versioned envelope (owned form, for loading).
#[derive(Debug, Deserialize)]
struct Persisted {
    version: u32,
    entries: HashMap<String, FileCache>, // key = project-relative path
}

/// Borrowing form, for saving without cloning the whole map.
#[derive(Serialize)]
struct PersistedRef<'a> {
    version: u32,
    entries: &'a HashMap<String, FileCache>,
}

/// In-memory cache keyed by project-relative path.
#[derive(Debug, Default)]
pub struct Store {
    entries: HashMap<String, FileCache>,
}

fn cache_path(root: &Path) -> PathBuf {
    root.join(".clew").join("cache").join("index.json")
}

impl Store {
    /// Load the cache for `root`, or an empty store when it is missing, corrupt,
    /// or from a different schema version (any of which forces a full rebuild).
    pub fn load(root: &Path) -> Store {
        let entries = std::fs::read_to_string(cache_path(root))
            .ok()
            .and_then(|s| serde_json::from_str::<Persisted>(&s).ok())
            .filter(|p| p.version == CACHE_VERSION)
            .map(|p| p.entries)
            .unwrap_or_default();
        Store { entries }
    }

    /// The cached record for a relative path, if present.
    pub fn get(&self, rel: &str) -> Option<&FileCache> {
        self.entries.get(rel)
    }

    pub fn insert(&mut self, rel: String, entry: FileCache) {
        self.entries.insert(rel, entry);
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Persist the cache under `<root>/.clew/cache/`. Best-effort: a write error
    /// (e.g. `.clew` removed) is surfaced but never corrupts anything, since a
    /// missing/partial cache just triggers a rebuild next time.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = cache_path(root);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Compact (not pretty): this is machine data and can be large.
        let json = serde_json::to_string(&PersistedRef {
            version: CACHE_VERSION,
            entries: &self.entries,
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        // Write to a temp file then rename, so a crash mid-write never leaves a
        // half-written cache that would fail to parse (and merely rebuild anyway,
        // but this keeps the on-disk file always valid).
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(hash: Version) -> FileCache {
        FileCache {
            mtime_ns: 1,
            size: 2,
            hash,
            symbols: vec![CachedSymbol {
                name: "foo".into(),
                kind: "function".into(),
                line: 3,
                is_test: false,
            }],
            imports: vec![CachedImport {
                module: "crate::bar".into(),
                line: 1,
                is_mod: false,
            }],
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let root = std::env::temp_dir().join("clew-cache-rt");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".clew")).unwrap();

        let mut store = Store::default();
        store.insert("src/a.rs".into(), entry(42));
        store.save(&root).unwrap();

        let loaded = Store::load(&root);
        assert_eq!(loaded.len(), 1);
        let e = loaded.get("src/a.rs").unwrap();
        assert_eq!(e.hash, 42);
        assert_eq!(e.symbols[0].name, "foo");
    }

    #[test]
    fn version_mismatch_and_corruption_yield_empty() {
        let root = std::env::temp_dir().join("clew-cache-bad");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".clew").join("cache")).unwrap();
        let path = cache_path(&root);

        // Wrong version → ignored.
        std::fs::write(&path, r#"{"version":999,"entries":{}}"#).unwrap();
        assert!(Store::load(&root).is_empty());

        // Corrupt JSON → ignored (rebuild), never a panic.
        std::fs::write(&path, "not json at all").unwrap();
        assert!(Store::load(&root).is_empty());

        // Missing file → empty.
        std::fs::remove_file(&path).unwrap();
        assert!(Store::load(&root).is_empty());
    }
}
