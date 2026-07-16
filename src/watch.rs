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

use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iced::Subscription;
use iced::futures::{SinkExt, Stream};
use notify_debouncer_full::new_debouncer;
use notify_debouncer_full::notify::{EventKind, RecursiveMode};

use crate::Message;

/// Debounce window: coalesces the burst a single save or `git pull` produces
/// into one batch. notify picks a tick rate of 1/4 of this when given `None`.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// A file whose bytes actually changed, with its fresh content (when it is a
/// readable UTF-8 text file we still care about).
#[derive(Debug, Clone)]
pub struct Changed {
    pub path: PathBuf,
    pub hash: u64,
    pub content: Arc<String>,
}

/// Fast 64-bit content hash for change detection. Not cryptographic — it only
/// needs to distinguish "same bytes" from "different bytes" cheaply.
pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(bytes);
    h.finish()
}

/// Re-read and re-hash a set of `(path, last_known_hash)` off the UI thread,
/// keeping only the ones whose bytes truly changed (and are readable text).
/// Blocking; run via `spawn_blocking`.
pub fn rehash(candidates: Vec<(PathBuf, u64)>) -> Vec<Changed> {
    candidates
        .into_iter()
        .filter_map(|(path, old)| {
            let bytes = std::fs::read(&path).ok()?;
            let hash = content_hash(&bytes);
            if hash == old {
                return None; // false positive — bytes unchanged
            }
            let content = String::from_utf8(bytes).ok()?; // skip binary
            Some(Changed {
                path,
                hash,
                content: Arc::new(content),
            })
        })
        .collect()
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
    fn hash_distinguishes_content() {
        assert_eq!(content_hash(b"abc"), content_hash(b"abc"));
        assert_ne!(content_hash(b"abc"), content_hash(b"abd"));
    }

    #[test]
    fn rehash_drops_unchanged_and_keeps_changed() {
        let dir = std::env::temp_dir().join("clew-watch-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        std::fs::write(&a, "one\n").unwrap();
        let h = content_hash(b"one\n");

        // Unchanged bytes → dropped.
        assert!(rehash(vec![(a.clone(), h)]).is_empty());

        // Changed bytes → returned with new content.
        std::fs::write(&a, "two\n").unwrap();
        let out = rehash(vec![(a.clone(), h)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content.as_str(), "two\n");
        assert_ne!(out[0].hash, h);
    }

    #[test]
    fn noise_paths_are_skipped() {
        assert!(is_noise(Path::new("/p/target/debug/x")));
        assert!(is_noise(Path::new("/p/.git/HEAD")));
        assert!(is_noise(Path::new("/p/.clew/bookmarks.json")));
        assert!(!is_noise(Path::new("/p/src/main.rs")));
    }
}
