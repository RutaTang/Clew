//! Content-refresh helpers for on-disk changes.
//!
//! Watching the filesystem now happens on clew-server, which streams
//! `FilesChanged` / `Tree` notifications (see `handle_server_event`). What
//! remains here is the byte-level classification the legacy in-process path
//! still uses: given candidate paths, decide what actually changed by comparing
//! bytes ([`rehash`]) or existence ([`structural_changes`]).

use std::path::{Path, PathBuf};
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
/// bytes are unchanged (a false positive), unreadable as text, or larger than
/// `max_bytes`. `0` is a fine "unknown" sentinel for a not-yet-tracked path — a
/// real hash is never `0` by intent, and an unlucky collision merely skips one
/// refresh. Blocking; run via `spawn_blocking`.
///
/// The cap is not optional. This runs on whatever the watcher reports, so a
/// single multi-gigabyte generated source file dropped into the tree is enough
/// to exhaust memory here — and everything downstream (hash, highlight, index)
/// would then work on it too.
///
/// Neither is the `root` check. An open source file can be replaced in place
/// by something that is no longer an ordinary file inside the project: a
/// symlink pointing out of it, whose contents this would then display under a
/// project path, or a FIFO, whose `open` blocks with no writer and wedges this
/// whole blocking task for the life of the process.
pub fn rehash(root: &Path, candidates: Vec<(PathBuf, Version)>, max_bytes: u64) -> Vec<FileEvent> {
    use std::io::Read;
    candidates
        .into_iter()
        .filter_map(|(path, old)| {
            // Classify before opening. `is_inside` covers both halves —
            // `symlink_metadata` + `is_file` rejects symlinks, FIFOs, devices
            // and directories without following anything, and the canonicalized
            // containment check rejects a path that no longer belongs to this
            // project. Same gate `graph::index` already applies per file.
            if !clew_core::fs_scan::is_inside(root, &path) {
                // A tracked file that is no longer an ordinary project file is
                // gone as far as the viewer is concerned. Silently keeping the
                // pre-swap bytes on screen would be worse than saying so.
                return (old != 0).then_some(FileEvent::Deleted(path));
            }
            let file = match std::fs::File::open(&path) {
                Err(_) if old != 0 => return Some(FileEvent::Deleted(path)), // tracked, now gone
                Err(_) => return None, // never existed — ignore
                Ok(f) => f,
            };
            // fstat on the handle we will read, before a single byte of it.
            if file.metadata().ok()?.len() > max_bytes {
                return None;
            }
            // Through the cap as well: the size above is a cheap early
            // rejection, but a file can grow between the fstat and the read.
            let mut bytes = Vec::new();
            file.take(max_bytes + 1).read_to_end(&mut bytes).ok()?;
            if bytes.len() as u64 > max_bytes {
                return None;
            }
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

        const CAP: u64 = 1024 * 1024;

        // Unchanged bytes → dropped.
        assert!(rehash(&dir, vec![(a.clone(), h)], CAP).is_empty());

        // Changed bytes → Modified with fresh content.
        std::fs::write(&a, "two\n").unwrap();
        let out = rehash(&dir, vec![(a.clone(), h)], CAP);
        assert!(matches!(&out[..], [FileEvent::Modified(c)] if c.content.as_str() == "two\n"));

        // Removed while tracked → Deleted.
        std::fs::remove_file(&a).unwrap();
        let out = rehash(&dir, vec![(a.clone(), content_hash(b"two\n"))], CAP);
        assert!(matches!(&out[..], [FileEvent::Deleted(p)] if p == &a));
    }

    /// A tracked file replaced in place by a symlink out of the project must
    /// not have the target's bytes displayed under the project path, and one
    /// replaced by a FIFO must not wedge the blocking task: `File::open` on a
    /// FIFO with no writer blocks forever.
    #[test]
    #[cfg(unix)]
    fn rehash_refuses_a_swapped_symlink_and_fifo() {
        const CAP: u64 = 1024 * 1024;
        let dir = std::env::temp_dir().join("clew-watch-swap-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let outside = std::env::temp_dir().join("clew-watch-swap-outside.txt");
        std::fs::write(&outside, "secret\n").unwrap();

        let link = dir.join("link.rs");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let tracked = content_hash(b"was a real file\n");
        let out = rehash(&dir, vec![(link.clone(), tracked)], CAP);
        assert!(
            matches!(&out[..], [FileEvent::Deleted(p)] if p == &link),
            "symlink out of the project must not be read: {out:?}"
        );

        let pipe = dir.join("pipe.rs");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&pipe)
                .status()
                .is_ok_and(|s| s.success()),
            "mkfifo is needed for this test"
        );
        // On a thread, so a regression shows up as a failed assert rather than
        // a test run that never finishes.
        let (tx, rx) = std::sync::mpsc::channel();
        let (d, p) = (dir.clone(), pipe.clone());
        std::thread::spawn(move || {
            let _ = tx.send(rehash(&d, vec![(p, tracked)], CAP));
        });
        let out = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("rehash blocked opening the FIFO");
        assert!(matches!(&out[..], [FileEvent::Deleted(p)] if p == &pipe));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&outside);
    }

    /// A file past the cap is dropped, not read: the refresh pipeline never
    /// sees it, and nothing downstream gets a chance to hold it in memory.
    #[test]
    fn rehash_drops_files_over_the_cap() {
        let dir = std::env::temp_dir().join("clew-watch-cap-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.txt");
        std::fs::write(&big, vec![b'a'; 4096]).unwrap();

        assert!(rehash(&dir, vec![(big.clone(), 0)], 1024).is_empty());
        // The same file is reported normally once the cap allows it.
        assert!(matches!(
            &rehash(&dir, vec![(big.clone(), 0)], 8192)[..],
            [FileEvent::Modified(_)]
        ));
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
