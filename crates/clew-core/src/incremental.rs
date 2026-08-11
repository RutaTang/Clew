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
    /// Paths an off-thread read is currently hashing, counted because batches
    /// overlap over the same file. A slot being read is one a background pass
    /// must not fill: see [`Registry::begin_read`].
    reading: HashMap<PathBuf, usize>,
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

    /// Note that an off-thread read of these paths is in flight, hashed
    /// against whatever version is recorded right now.
    ///
    /// That baseline is the reader's only ordering evidence when its result
    /// lands, so an empty slot it was dispatched against must stay empty:
    /// filling it from an older pass makes the arriving read look superseded
    /// and get dropped, which is the older read winning. Counted rather than a
    /// set because two batches can be reading the same file at once; every
    /// dispatched path must be released again with [`Registry::end_read`].
    pub fn begin_read(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for path in paths {
            *self.reading.entry(path).or_insert(0) += 1;
        }
    }

    /// Release reads noted by [`Registry::begin_read`]. Unknown paths are
    /// ignored, so a result the app assembled itself is harmless.
    pub fn end_read<'a>(&mut self, paths: impl IntoIterator<Item = &'a Path>) {
        for path in paths {
            if let Some(n) = self.reading.get_mut(path) {
                *n -= 1;
                if *n == 0 {
                    self.reading.remove(path);
                }
            }
        }
    }

    /// Whether an off-thread read of this path is in flight.
    pub fn is_reading(&self, path: &Path) -> bool {
        self.reading.contains_key(path)
    }

    /// Bulk-seed from a background hash pass (one revision bump for the batch).
    ///
    /// A seed describes the tree as it was when that pass STARTED, so it is
    /// never authoritative over an entry the registry already holds: every
    /// other writer — the watcher's change dispatch, and the pane load that
    /// records the bytes it has just read — took its read later than this
    /// pass did. A tracked path is therefore kept, and the ones whose seeded
    /// version disagreed are returned so the caller can leave the rest of its
    /// batch (symbols, imports, whatever else that pass derived from those
    /// bytes) out too. Overwriting them rolled files back to bytes no longer
    /// on disk.
    ///
    /// A path with a read in flight is skipped for the same reason one step
    /// earlier: it is untracked only because nobody has landed a read of it
    /// yet, and seeding it now would move the slot out from under the reader's
    /// baseline and cost the fresher read instead. It is not reported as
    /// rejected — the caller's symbols for it are this pass's, which the
    /// arriving read overwrites, and which are all there is if that read turns
    /// out to carry nothing (an oversized or binary file yields no event).
    ///
    /// DELETION IS INVISIBLE HERE, and the caller must filter it out itself.
    /// The registry keeps no tombstones, so a file deleted while the pass ran
    /// and one it had simply never tracked are the same empty slot, and this
    /// insert brings the dead path back at its pre-deletion hash. Only the
    /// caller holds the newer file list that says which is which.
    pub fn seed(&mut self, hashes: impl IntoIterator<Item = (PathBuf, Version)>) -> Vec<PathBuf> {
        let mut rejected = Vec::new();
        // Tracked explicitly rather than by comparing `len()` before and
        // after: a key count only notices insertions, so any future change to
        // an existing entry would slip through without bumping the revision,
        // and every lazy consumer keyed on it would keep serving stale data.
        let mut changed = false;
        for (path, hash) in hashes {
            if self.reading.contains_key(&path) {
                continue;
            }
            match self.versions.get(&path) {
                Some(&known) if known != hash => rejected.push(path),
                Some(_) => {}
                None => {
                    self.versions.insert(path, hash);
                    changed = true;
                }
            }
        }
        if changed {
            self.revision += 1;
        }
        rejected
    }

    /// Clear everything (project close / switch). The in-flight reads go too:
    /// their results are dropped as another project's, so nothing would ever
    /// release them and every one of their slots would stay unseedable.
    pub fn clear(&mut self) {
        self.versions.clear();
        self.reading.clear();
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
        let rev = r.revision();
        let rejected = r.seed(vec![
            (PathBuf::from("/p/a.rs"), 10),
            (PathBuf::from("/p/b.rs"), 20),
        ]);
        assert!(rejected.is_empty());
        assert_eq!(r.len(), 2);
        assert_eq!(r.version(Path::new("/p/b.rs")), Some(20));
        assert_eq!(r.revision(), rev + 1, "one bump for the whole batch");
    }

    #[test]
    fn seed_never_rolls_a_tracked_file_back() {
        // The background pass hashes the tree over some span of time, and the
        // change dispatch keeps writing while it runs. A seed landing after
        // one of those writes carries the OLDER bytes for that file, so it
        // must lose — and say which paths it lost on, because the pass's other
        // derived data for them is just as stale.
        let mut r = Registry::default();
        let edited = PathBuf::from("/p/a.rs");
        let untouched = PathBuf::from("/p/b.rs");
        r.set(edited.clone(), 99);
        let rev = r.revision();

        let rejected = r.seed(vec![(edited.clone(), 10), (untouched.clone(), 20)]);
        assert_eq!(rejected, vec![edited.clone()]);
        assert_eq!(r.version(&edited), Some(99));
        assert_eq!(
            r.version(&untouched),
            Some(20),
            "untouched files still seed"
        );
        assert_eq!(r.revision(), rev + 1);

        // Re-seeding what is already recorded changes nothing at all, so the
        // revision holds and consumers keyed on it do not recompute.
        let rev = r.revision();
        assert!(r.seed(vec![(untouched.clone(), 20)]).is_empty());
        assert_eq!(r.revision(), rev);
    }

    #[test]
    fn seed_leaves_a_slot_a_read_is_in_flight_for_empty() {
        // The change dispatch hashes a file against the version recorded when
        // it went out, and that is all its result has to prove it is not an
        // older read landing late. Seeding the empty slot it was dispatched
        // against would make the fresher read look superseded and lose.
        let mut r = Registry::default();
        let reading = PathBuf::from("/p/a.rs");
        let idle = PathBuf::from("/p/b.rs");
        r.begin_read([reading.clone()]);
        assert!(r.is_reading(&reading));

        let rejected = r.seed(vec![(reading.clone(), 10), (idle.clone(), 20)]);
        assert!(
            rejected.is_empty(),
            "a skipped slot is not stale: this pass's symbols stand until the read lands"
        );
        assert_eq!(
            r.version(&reading),
            None,
            "the reader's baseline must still be the one it was dispatched with"
        );
        assert_eq!(r.version(&idle), Some(20));

        // Once the read has landed the slot seeds like any other.
        r.end_read([reading.as_path()]);
        assert!(!r.is_reading(&reading));
        r.seed(vec![(reading.clone(), 10)]);
        assert_eq!(r.version(&reading), Some(10));
    }

    #[test]
    fn overlapping_reads_release_one_at_a_time() {
        // Two batches can be reading the same file; the first to land must not
        // free the slot out from under the second, or a seed in between takes
        // it and the second read is dropped as superseded.
        let mut r = Registry::default();
        let p = PathBuf::from("/p/a.rs");
        r.begin_read([p.clone(), p.clone()]);
        r.end_read([p.as_path()]);
        assert!(r.is_reading(&p), "one batch is still reading");
        r.end_read([p.as_path()]);
        assert!(!r.is_reading(&p));
        // Releasing a path nobody claimed is a no-op, not a panic: a result
        // assembled without a dispatch (tests, replays) still passes through.
        r.end_read([p.as_path()]);
        assert!(!r.is_reading(&p));
    }
}
