//! File-system watcher: turns on-disk changes to the project into debounced
//! `FilesChanged` messages.
//!
//! This layer only *detects that something might have changed* and forwards the
//! candidate paths. The authoritative change decision is made downstream by a
//! content-hash comparison in the app (see [`content_hash`] and `rehash`), so
//! the noise a watcher inevitably produces — atomic saves, editor lock files,
//! `mtime` touches from `git checkout` — is filtered by comparing bytes, not by
//! trusting the event. Change *propagation* (which derived data to invalidate)
//! is likewise the app's job; the watcher knows nothing about dependencies.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iced::Subscription;
use iced::futures::{SinkExt, Stream};
use notify_debouncer_full::new_debouncer;
use notify_debouncer_full::notify::{EventKind, RecursiveMode};

use crate::Message;
use crate::incremental::{Version, content_hash};

/// Debounce window: coalesces the burst a single save or `git pull` produces
/// into one batch. notify picks a tick rate of 1/4 of this when given `None`.
const DEBOUNCE: Duration = Duration::from_millis(250);

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
/// This is how non-source files (a `.txt`, a `.json`) — which carry no content
/// we display and so are never re-hashed — stay live in the tree without a
/// restart. Blocking; run via `spawn_blocking`.
pub fn structural_changes(probes: &[(PathBuf, bool)]) -> bool {
    probes.iter().any(|(path, in_tree)| {
        let exists = std::fs::symlink_metadata(path).is_ok();
        exists != *in_tree
    })
}

/// Watch `root` recursively and emit `Message::FilesChanged` batches. Keyed on
/// `root` so switching projects tears down the old watcher and starts a new one.
pub fn watch(root: PathBuf) -> Subscription<Message> {
    Subscription::run_with(root, build_stream)
}

/// Plain `fn` (no captures) as required by `Subscription::run_with`. `use<>`
/// keeps the returned stream from capturing the `&PathBuf` lifetime — it owns a
/// clone, so it is `'static`, which `run_with` requires. The `&PathBuf` (not
/// `&Path`) is forced by `run_with`'s `fn(&D)` where `D = PathBuf`.
#[allow(clippy::ptr_arg)]
fn build_stream(root: &PathBuf) -> impl Stream<Item = Message> + use<> {
    let root = root.clone();
    iced::stream::channel(64, move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
        // The debouncer runs on its own thread; bridge its callback into this
        // async task with a channel.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<PathBuf>>();

        let debouncer = new_debouncer(DEBOUNCE, None, move |res: notify_debouncer_full::DebounceEventResult| {
            let Ok(events) = res else { return };
            let mut paths: Vec<PathBuf> = Vec::new();
            for ev in events {
                if !relevant(&ev.kind) {
                    continue;
                }
                for p in &ev.paths {
                    if !is_noise(p) {
                        paths.push(p.clone());
                    }
                }
            }
            if !paths.is_empty() {
                let _ = tx.send(paths);
            }
        });

        let Ok(mut debouncer) = debouncer else {
            return; // watcher backend unavailable — degrade to no live refresh
        };
        if debouncer.watch(&root, RecursiveMode::Recursive).is_err() {
            return;
        }

        while let Some(paths) = rx.recv().await {
            if output.send(Message::FilesChanged(paths)).await.is_err() {
                break; // the app dropped the receiver (closing / project change)
            }
        }
        drop(debouncer); // hold the watcher alive until the stream ends
    })
}

/// Only content-affecting events are worth a re-hash.
fn relevant(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Skip VCS internals, build output, dependencies and clew's own data dir, so a
/// `cargo build` or `npm install` doesn't drown the channel.
fn is_noise(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some(".git") | Some("target") | Some("node_modules") | Some(".clew") | Some(".hg")
                | Some(".svn") | Some(".idea") | Some(".DS_Store")
        )
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
        assert!(structural_changes(&[(present.clone(), true), (absent.clone(), true)]));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn noise_paths_are_skipped() {
        assert!(is_noise(Path::new("/p/target/debug/x")));
        assert!(is_noise(Path::new("/p/.git/HEAD")));
        assert!(is_noise(Path::new("/p/.clew/bookmarks.json")));
        assert!(!is_noise(Path::new("/p/src/main.rs")));
    }
}
