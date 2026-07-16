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
