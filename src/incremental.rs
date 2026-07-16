//! The incremental / invalidation core.
//!
//! Every artifact clew derives — syntax highlighting, the symbol index, and
//! later call graphs and LLM explanations — is a pure function of file (and
//! symbol) contents. To keep those artifacts fresh as the codebase changes
//! underneath the reader, without recomputing everything or trusting noisy
//! filesystem events, this module is the single source of truth for *what has
//! changed*: it hashes inputs so any consumer can tell fresh from stale cheaply.
//!
//! The design deliberately separates the two hard problems:
//!   * **Detection** — did an input's bytes change? — is answered here by a
//!     content hash, never by mtime or by trusting the watcher's events.
//!   * **Propagation** — which derived data does a change invalidate? — is the
//!     consumer's job: a per-file artifact matches by path; a cross-file
//!     artifact (call graph, LLM) records the inputs it read and re-checks their
//!     versions. The [`Registry`] is the version oracle both rely on.

use std::collections::HashMap;
use std::hash::Hasher;
use std::path::{Path, PathBuf};

/// A content version: the hash of an input's bytes. Equal ⇒ unchanged. Not
/// cryptographic — it only needs to distinguish "same bytes" from "different".
pub type Version = u64;

/// Fast 64-bit content hash for change detection.
pub fn content_hash(bytes: &[u8]) -> Version {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(bytes);
    h.finish()
}

/// Whole-project content-hash registry: the authority on whether a file's bytes
/// changed. Seeded by a background pass after a scan and kept current by the
/// watcher's change dispatch. `revision` bumps on every real change so lazy
/// consumers can cheaply tell "has anything changed since I last looked".
#[derive(Debug, Default)]
pub struct Registry {
    versions: HashMap<PathBuf, Version>,
    revision: u64,
}

impl Registry {
    /// Current version of a file, if tracked.
    pub fn version(&self, path: &Path) -> Option<Version> {
        self.versions.get(path).copied()
    }

    /// Monotonic revision, bumped on every create / modify / delete.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_tracked(&self, path: &Path) -> bool {
        self.versions.contains_key(path)
    }

    pub fn len(&self) -> usize {
        self.versions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.versions.is_empty()
    }

    /// Record a file's current hash. Returns `true` when it is new or actually
    /// changed (in which case the revision is bumped).
    pub fn set(&mut self, path: PathBuf, hash: Version) -> bool {
        if self.versions.get(&path) == Some(&hash) {
            return false;
        }
        self.versions.insert(path, hash);
        self.revision += 1;
        true
    }

    /// Forget a deleted file. Returns `true` if it had been tracked.
    pub fn remove(&mut self, path: &Path) -> bool {
        if self.versions.remove(path).is_some() {
            self.revision += 1;
            true
        } else {
            false
        }
    }

    /// Bulk-seed from a background hash pass (one revision bump for the batch).
    pub fn seed(&mut self, hashes: impl IntoIterator<Item = (PathBuf, Version)>) {
        let before = self.versions.len();
        for (path, hash) in hashes {
            self.versions.insert(path, hash);
        }
        if self.versions.len() != before {
            self.revision += 1;
        }
    }

    /// Clear everything (project close / switch).
    pub fn clear(&mut self) {
        self.versions.clear();
        self.revision += 1;
    }
}

/// Per-symbol content hashes for one file, keyed by `"kind:name"`, each hashing
/// the *text of the symbol's definition span* (not its position). This gives
/// precise, position-independent invalidation: a consumer that explains or
/// analyses a function (call graph, LLM) recomputes only when that function's
/// own text changed — inserting a line above it, or editing a sibling, leaves
/// its hash untouched. Overloaded names collapse to one key (their hashes are
/// xor-merged), a safe over-approximation.
///
/// Ready symbol-level invalidation API; its first consumers are the call graph
/// and LLM explanations (which must not recompute on unrelated edits).
#[allow(dead_code)]
pub fn symbol_hashes(source: &str, lang: &'static str) -> HashMap<String, Version> {
    let lines: Vec<&str> = source.lines().collect();
    let mut out: HashMap<String, Version> = HashMap::new();
    for sym in crate::outline::extract(source, lang) {
        let start = sym.line.saturating_sub(1);
        let end = sym.end_line.min(lines.len());
        let span = lines.get(start..end).unwrap_or(&[]).join("\n");
        let h = content_hash(span.as_bytes());
        out.entry(format!("{}:{}", sym.kind, sym.name))
            .and_modify(|v| *v ^= h)
            .or_insert(h);
    }
    out
}

/// The symbol keys that differ between two versions of a file's symbol hashes:
/// added, removed, or whose span changed. This is what a symbol-level consumer
/// marks dirty when a file changes.
#[allow(dead_code)]
pub fn changed_symbols(
    old: &HashMap<String, Version>,
    new: &HashMap<String, Version>,
) -> Vec<String> {
    let mut changed: Vec<String> = new
        .iter()
        .filter(|(k, v)| old.get(*k) != Some(*v))
        .map(|(k, _)| k.clone())
        .collect();
    changed.extend(old.keys().filter(|k| !new.contains_key(*k)).cloned());
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_distinguishes_bytes() {
        assert_eq!(content_hash(b"fn a() {}"), content_hash(b"fn a() {}"));
        assert_ne!(content_hash(b"fn a() {}"), content_hash(b"fn b() {}"));
    }

    #[test]
    fn registry_tracks_versions_and_revision() {
        let mut r = Registry::default();
        let p = PathBuf::from("/p/a.rs");
        assert_eq!(r.version(&p), None);

        // First set is a change; the revision advances.
        assert!(r.set(p.clone(), 1));
        let rev1 = r.revision();
        assert_eq!(r.version(&p), Some(1));

        // Same hash is not a change; revision holds.
        assert!(!r.set(p.clone(), 1));
        assert_eq!(r.revision(), rev1);

        // New hash is a change.
        assert!(r.set(p.clone(), 2));
        assert!(r.revision() > rev1);

        // Removal advances the revision and forgets the file.
        assert!(r.remove(&p));
        assert_eq!(r.version(&p), None);
        assert!(!r.remove(&p)); // already gone
    }

    #[test]
    fn symbol_hashes_are_span_based_and_position_independent() {
        let base = "fn a() {\n    1\n}\nfn b() {\n    2\n}\n";
        let h1 = symbol_hashes(base, "rust");
        assert!(h1.contains_key("function:a") && h1.contains_key("function:b"));

        // Changing b's body changes only b.
        let edited_b = "fn a() {\n    1\n}\nfn b() {\n    999\n}\n";
        let h2 = symbol_hashes(edited_b, "rust");
        assert_eq!(h1["function:a"], h2["function:a"]);
        assert_ne!(h1["function:b"], h2["function:b"]);
        assert_eq!(changed_symbols(&h1, &h2), vec!["function:b".to_string()]);

        // Inserting a line above a leaves a's hash untouched (position-independent).
        let shifted = "// a new comment line\nfn a() {\n    1\n}\nfn b() {\n    2\n}\n";
        let h3 = symbol_hashes(shifted, "rust");
        assert_eq!(h1["function:a"], h3["function:a"]);
        assert_eq!(h1["function:b"], h3["function:b"]);
        assert!(changed_symbols(&h1, &h3).is_empty());
    }

    #[test]
    fn changed_symbols_reports_removed() {
        let old = symbol_hashes("fn a() {}\nfn b() {}\n", "rust");
        let new = symbol_hashes("fn a() {}\n", "rust");
        assert_eq!(changed_symbols(&old, &new), vec!["function:b".to_string()]);
    }

    #[test]
    fn seed_populates_without_per_entry_bumps() {
        let mut r = Registry::default();
        r.seed(vec![
            (PathBuf::from("/p/a.rs"), 10),
            (PathBuf::from("/p/b.rs"), 20),
        ]);
        assert_eq!(r.len(), 2);
        assert_eq!(r.version(Path::new("/p/b.rs")), Some(20));
    }
}
