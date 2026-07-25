//! Content-refresh helpers for on-disk changes.
//!
//! Watching the filesystem now happens on clew-server, which streams
//! `FilesChanged` / `Tree` notifications (see `handle_server_event`). What
//! remains here is the byte-level classification the legacy in-process path
//! still uses: given candidate paths, decide what actually changed by comparing
//! bytes ([`rehash`]) or existence ([`structural_changes`]).

use std::path::PathBuf;
use std::sync::Arc;

use crate::incremental::{Version, content_hash};

/// A file whose bytes actually changed, with its fresh content.
#[derive(Debug, Clone)]
pub struct Changed {
    pub path: PathBuf,
    pub hash: Version,
    pub content: Arc<String>,
}

/// The verified outcome of re-hashing a watched path.
#[derive(Debug, Clone)]
pub enum FileEvent {
    /// Bytes changed (or the file is newly created): carries fresh content.
    Modified(Changed),
    /// The file no longer exists on disk.
    Deleted(PathBuf),
}

/// Re-read and re-hash a set of `(path, last_known_hash)` off the UI thread,
/// classifying each as a real modification, a deletion, or dropping it when the
/// bytes are unchanged (a false positive) or unreadable as text. `0` is a fine
/// "unknown" sentinel for a not-yet-tracked path — a real hash is never `0` by
/// intent, and an unlucky collision merely skips one refresh. Blocking; run via
/// `spawn_blocking`.
pub fn rehash(candidates: Vec<(PathBuf, Version)>) -> Vec<FileEvent> {
    candidates
        .into_iter()
        .filter_map(|(path, old)| match std::fs::read(&path) {
            Err(_) if old != 0 => Some(FileEvent::Deleted(path)), // was tracked, now gone
            Err(_) => None,                                       // never existed — ignore
            Ok(bytes) => {
                let hash = content_hash(&bytes);
                if hash == old {
                    return None; // false positive — bytes unchanged
                }
                let content = String::from_utf8(bytes).ok()?; // skip binary
                Some(FileEvent::Modified(Changed {
                    path,
                    hash,
                    content: Arc::new(content),
                }))
            }
        })
        .collect()
}

/// Decide whether any changed path represents a *structural* change to the file
/// tree — a file created or deleted — using a cheap existence check (a `stat`,
/// never a read). Each probe pairs a path with whether the tree currently lists
/// it; when on-disk existence disagrees with that, the path was just created
/// (exists, not listed) or deleted (gone, still listed), so the tree is stale.
/// Blocking; run via `spawn_blocking`.
pub fn structural_changes(probes: &[(PathBuf, bool)]) -> bool {
    probes.iter().any(|(path, in_tree)| {
        let exists = std::fs::symlink_metadata(path).is_ok();
        exists != *in_tree
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rehash_classifies_unchanged_modified_and_deleted() {
        let dir = std::env::temp_dir().join("clew-watch-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        std::fs::write(&a, "one\n").unwrap();
        let h = content_hash(b"one\n");

        // Unchanged bytes → dropped.
        assert!(rehash(vec![(a.clone(), h)]).is_empty());

        // Changed bytes → Modified with fresh content.
        std::fs::write(&a, "two\n").unwrap();
        let out = rehash(vec![(a.clone(), h)]);
        assert!(matches!(&out[..], [FileEvent::Modified(c)] if c.content.as_str() == "two\n"));

        // Removed while tracked → Deleted.
        std::fs::remove_file(&a).unwrap();
        let out = rehash(vec![(a.clone(), content_hash(b"two\n"))]);
        assert!(matches!(&out[..], [FileEvent::Deleted(p)] if p == &a));
    }

    #[test]
    fn structural_changes_flags_creates_and_deletes_only() {
        let dir = std::env::temp_dir().join("clew-structural-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let present = dir.join("present.txt");
        let absent = dir.join("absent.txt");
        std::fs::write(&present, "x").unwrap();

        // Exists on disk but the tree doesn't list it → a creation.
        assert!(structural_changes(&[(present.clone(), false)]));
        // Listed in the tree but gone from disk → a deletion.
        assert!(structural_changes(&[(absent.clone(), true)]));
        // Edit (exists and already listed) or transient (gone and unlisted) →
        // not structural, so no needless rescan.
        assert!(!structural_changes(&[(present.clone(), true)]));
        assert!(!structural_changes(&[(absent.clone(), false)]));
        // Any structural path in the batch wins.
        assert!(structural_changes(&[
            (present.clone(), true),
            (absent.clone(), true)
        ]));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
