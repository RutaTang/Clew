//! Persistent warm-start cache for the symbol index and content hashes, stored
//! in the project's derived-artifact directory (`clew_core::derived::dir`,
//! inside clew's own data directory — never inside the project).
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
//!     hash changes.** (The content hash itself is frozen — see
//!     `clew_core::incremental::content_hash` — so a toolchain upgrade alone
//!     never changes it.)
//!   * Any read/parse/version error falls back to an empty cache (full rebuild).
//!     Unlike the user's own stores, overwriting an unreadable cache is fine:
//!     it holds nothing that cannot be derived again.
//!   * It lives in clew's data directory, keyed by project, so a repository
//!     can never ship one: every entry is keyed by a content hash the
//!     repository could compute for its own files, and a committed cache would
//!     otherwise be accepted as clew's own work.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::incremental::Version;

/// Bump on any change to symbol/import extraction or the content hash so stale
/// caches from an older clew are discarded rather than trusted.
///
/// 6: Rust imports are rescoped per inline module, `use` groups are expanded,
/// and CommonJS / dynamic-import specifiers are extracted, so an index built by
/// an older clew describes different edges.
/// 7: Rust specifiers record re-exports (`pub `) and globs (`::*`), and Python
/// `from . import x` is recorded as `.:x` (`clew_core::rustscope`,
/// `clew_core::imports::PY_NAME_SEP`).
/// 8: each file's call sites (and the lines of its bodyless callables) are
/// cached beside its symbols ([`CachedCalls`]), so the project call graph is
/// built from the index instead of a second parse of every file; an older
/// entry has none to offer.
/// 9: the definition spans of a file's same-name callables are cached with
/// its call sites ([`CachedCalls::bodies`]), which tell a call after a nested
/// same-name function from one inside it; an older entry would resolve such
/// a call by the old rule.
/// 10: Rust `const` and `static` items are symbols (kind `constant`), so a
/// `use crate::LIMIT` resolves to the module defining the const instead of
/// being inferred; an older entry lists none of them.
pub(crate) const CACHE_VERSION: u32 = 10;

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

/// A file's call sites as cached (`clew_core::projectcalls::FileCalls`).
///
/// Compact on purpose. A file makes many more calls than it defines symbols,
/// and `index.json` is read through a 64 MiB cap
/// (`clew_core::statefile::MAX_STATE_BYTES`) past which the WHOLE cache is
/// ignored and every open parses the project again — so each name is stored
/// once per file and each call site is three small numbers in one flat
/// array, instead of an object repeating its field names and both names.
/// (On clew's own tree: about 13 bytes a call site, names included — 57k
/// sites add 0.75 MB to a 0.54 MB index.)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedCalls {
    /// The language the file was PARSED as (`Lang::key`): a C++ `.h` reads
    /// as C++, which its extension alone does not say.
    pub lang: String,
    /// Every name a call site mentions, as its caller or its callee, once.
    pub names: Vec<String>,
    /// Three numbers per call site, in the file's order: the caller (its
    /// index in `names` plus one; `0` for a call outside any function), the
    /// callee (its index in `names` times two, plus one for a
    /// `receiver.name(…)` call), and the line as its distance from the
    /// previous site's line — small, since sites come in source order.
    pub sites: Vec<i64>,
    /// 1-based lines of the file's callables declared without a body.
    pub declarations: Vec<usize>,
    /// Three numbers per definition of a name several callables of the file
    /// share (`clew_core::projectcalls::FileCalls::bodies`), by line: its
    /// definition line, then the first and last lines of the whole
    /// definition. Empty for most files.
    #[serde(default)]
    pub bodies: Vec<usize>,
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
    /// The call sites, for a file whose language has a call model and that
    /// makes or declares any; `None` otherwise.
    #[serde(default)]
    pub calls: Option<CachedCalls>,
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

fn cache_path(store: &Path) -> PathBuf {
    store.join("index.json")
}

impl Store {
    /// Load the cache for `root`, or an empty store when it is missing, corrupt,
    /// or from a different schema version (any of which forces a full rebuild).
    pub fn load(store: &Path) -> Store {
        let entries = clew_core::statefile::read(&cache_path(store))
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

    /// Persist the cache into the derived-artifact directory `store`.
    /// Best-effort: a write error is surfaced but never corrupts anything,
    /// since a missing/partial cache just triggers a rebuild next time.
    pub fn save(&self, store: &Path) -> std::io::Result<()> {
        // Compact (not pretty): this is machine data and can be large.
        let json = serde_json::to_string(&PersistedRef {
            version: CACHE_VERSION,
            entries: &self.entries,
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        clew_core::statefile::write_atomic(&cache_path(store), json.as_bytes())
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
            calls: Some(CachedCalls {
                lang: "rust".into(),
                names: vec!["foo".into(), "bar".into()],
                sites: vec![1, 2, 4],
                declarations: Vec::new(),
                bodies: vec![3, 3, 9],
            }),
        }
    }

    #[test]
    fn round_trips_through_disk() {
        // The store is a derived-artifact directory (never a project).
        let root = clew_core::testutil::TempDir::new("cache-rt");

        let mut store = Store::default();
        store.insert("src/a.rs".into(), entry(42));
        store.save(&root).unwrap();

        let loaded = Store::load(&root);
        assert_eq!(loaded.len(), 1);
        let e = loaded.get("src/a.rs").unwrap();
        assert_eq!(e.hash, 42);
        assert_eq!(e.symbols[0].name, "foo");
        assert_eq!(e.calls, entry(42).calls, "the call sites survive the trip");
    }

    #[test]
    fn version_mismatch_and_corruption_yield_empty() {
        let root = clew_core::testutil::TempDir::new("cache-bad");
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
