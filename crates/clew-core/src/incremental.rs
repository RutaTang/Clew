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
use std::path::{Path, PathBuf};

/// A content version: the hash of an input's bytes. Equal ⇒ unchanged. Not
/// cryptographic — it only needs to distinguish "same bytes" from "different".
pub type Version = u64;

/// Fast 64-bit content hash for change detection — and a PERSISTED key: the
/// explanation cache, the embedding index, the overview and the rendered-SVG
/// cache all file their (paid-for) results under it, so it must never change
/// between builds.
///
/// It is SipHash-1-3 with the all-zero key, written out here rather than
/// borrowed from `std`. That is precisely what `DefaultHasher::new()` computes
/// today, so every cache already on disk keeps matching — freezing the
/// algorithm costs no user a re-billed explanation — but `std` documents that
/// algorithm as unspecified and free to change in any release, and a toolchain
/// bump that changed it would have silently invalidated every cache at once.
/// Now only an edit to this function can, and the `content_hash_is_frozen`
/// test pins its output.
pub fn content_hash(bytes: &[u8]) -> Version {
    siphash13(0, 0, bytes)
}

/// SipHash-1-3 (one compression round, three finalization rounds) of `msg`
/// under the key `(k0, k1)`, as specified by Aumasson & Bernstein and as
/// implemented by Rust's `core::hash::sip::SipHasher13` for a single `write`.
fn siphash13(k0: u64, k1: u64, msg: &[u8]) -> u64 {
    #[inline(always)]
    fn round(v: &mut [u64; 4]) {
        v[0] = v[0].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(13);
        v[1] ^= v[0];
        v[0] = v[0].rotate_left(32);
        v[2] = v[2].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(16);
        v[3] ^= v[2];
        v[0] = v[0].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(21);
        v[3] ^= v[0];
        v[2] = v[2].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(17);
        v[1] ^= v[2];
        v[2] = v[2].rotate_left(32);
    }
    let mut v = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];
    let mut words = msg.chunks_exact(8);
    for word in &mut words {
        let m = u64::from_le_bytes(word.try_into().expect("an 8-byte chunk"));
        v[3] ^= m;
        round(&mut v);
        v[0] ^= m;
    }
    // The last block: the remaining bytes, little-endian, with the message
    // length (mod 256) in the top byte.
    let mut last = (msg.len() as u64) << 56;
    for (i, &byte) in words.remainder().iter().enumerate() {
        last |= u64::from(byte) << (8 * i);
    }
    v[3] ^= last;
    round(&mut v);
    v[0] ^= last;
    v[2] ^= 0xff;
    round(&mut v);
    round(&mut v);
    round(&mut v);
    v[0] ^ v[1] ^ v[2] ^ v[3]
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

    /// The persisted-key contract. These are the values `std`'s
    /// `DefaultHasher` produced (on Rust 1.95) when the algorithm was frozen,
    /// i.e. what every explanation, embedding, overview and SVG cache already
    /// on disk is filed under. If this fails, the change would silently
    /// invalidate — and re-bill — every user's cached LLM work: bump those
    /// caches' own versions deliberately instead.
    ///
    /// Inputs of every length from 0 to 16 bytes cover each tail size on both
    /// sides of a word boundary; the last two cover multi-block text.
    #[test]
    fn content_hash_is_frozen() {
        const PREFIXES: [u64; 17] = [
            0xd1fb_a762_150c_532c,
            0x68a9_1412_8e01_e473,
            0x010b_ac45_c41e_3669,
            0x4d4c_9a4a_8ef6_e0ad,
            0x7cc4_3f98_813e_4dbd,
            0x5abe_2169_dff3_6275,
            0xe3c2_5f87_624f_1cdb,
            0x2f09_8ab0_c751_325a,
            0xead4_11e6_7ebe_2eea,
            0x7592_7f9d_9512_4362,
            0xaf9f_77a6_5ab5_1a1d,
            0xfe64_ce8b_6617_fcff,
            0xa6ba_f4fb_0f9f_e1c2,
            0xa0cf_3211_850f_8e0d,
            0x7f86_0493_79fb_fe67,
            0xf30e_b725_bb91_c9ea,
            0x8972_1884_33a5_c5b7,
        ];
        let bytes: Vec<u8> = (0..16u8).collect();
        for (n, want) in PREFIXES.iter().enumerate() {
            assert_eq!(
                content_hash(&bytes[..n]),
                *want,
                "content_hash of the {n}-byte prefix moved"
            );
        }
        assert_eq!(content_hash(b"fn a() {}"), 0x86a8_bda7_7b7c_6a3e);
        assert_eq!(
            content_hash(
                "caf\u{e9} \u{1f600} a multi-word input spanning several blocks".as_bytes()
            ),
            0xee7b_e8fc_6169_1492
        );
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
