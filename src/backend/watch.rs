//! Content-refresh helpers for on-disk changes.
//!
//! Watching the filesystem now happens on clew-server, which streams
//! `FilesChanged` / `Tree` notifications (see `handle_server_event`). What
//! remains here is the byte-level classification the refresh path uses: given
//! candidate paths, decide what actually changed by comparing bytes
//! ([`rehash`]) or existence ([`structural_changes`]).
//!
//! A single `FilesChanged` can name thousands of files — a branch switch
//! rewrites the whole tree at once — so a batch is never read in one piece:
//! [`rehash_stream`] reads it in bounded chunks ([`ChunkLimits`]) on a
//! blocking thread and hands them over one at a time. Every chunk carries a
//! [`ChunkAck`], and the next chunk is not READ until the previous one's ack
//! is dropped — which the app does once it has applied the chunk. So one
//! chunk's contents exist at a time, however large the batch and however far
//! behind the UI falls. (Without the acknowledgement, only the stream's own
//! hand-offs bounded it, and iced's runtime keeps taking items past those
//! until its proxy is full: a UI far behind a huge batch could have a couple
//! of hundred chunks read ahead.)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use iced::futures::{SinkExt, Stream};

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
/// `max_bytes` (notebooks use their display cap instead). `0` is a fine
/// "unknown" sentinel for a not-yet-tracked path — a
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
            // A notebook opened by ReadFile can be up to 64 MiB because its
            // outputs are embedded in the JSON. Its watched bytes need the
            // same cap or ordinary external saves never reach the cell reload.
            let max_bytes = if clew_core::notebook::is_notebook(&path) {
                clew_core::notebook::MAX_NOTEBOOK_BYTES
            } else {
                max_bytes
            };
            // `is_inside` decides CONTAINMENT, and only that. It works from the
            // name — `symlink_metadata`, then two `canonicalize` calls — so what
            // it classifies is not what the next line opens: between the two, a
            // `rename(2)` can put a FIFO or an out-of-project symlink at the
            // same path, and a plain `File::open` carries neither `O_NOFOLLOW`
            // nor `O_NONBLOCK`, so it would follow the one and block forever in
            // `open(2)` on the other — parking this blocking worker for the life
            // of the process, with the batch's other events never delivered.
            if !clew_core::fs_scan::is_inside(root, &path) {
                // A tracked file that is no longer an ordinary project file is
                // gone as far as the viewer is concerned. Silently keeping the
                // pre-swap bytes on screen would be worse than saying so.
                return (old != 0).then_some(FileEvent::Deleted(path));
            }
            // So the TYPE is decided by the open itself, on the handle that is
            // actually read — the same `open_plain` every other project-source
            // reader in the tree now uses. It folds missing, symlink, FIFO and
            // every other non-regular case into `None`, which is exactly what
            // the containment branch above already does with them.
            let Some(file) = clew_core::statefile::open_plain(&path) else {
                return (old != 0).then_some(FileEvent::Deleted(path));
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

/// How much one rehash chunk may read: at most `files` candidates, and at most
/// `bytes` of their combined on-disk size (a single larger file gets a chunk
/// of its own; it is capped by the per-file limit anyway).
#[derive(Debug, Clone, Copy)]
pub struct ChunkLimits {
    pub files: usize,
    pub bytes: u64,
}

impl ChunkLimits {
    pub const DEFAULT: ChunkLimits = ChunkLimits {
        files: 256,
        bytes: 32 * 1024 * 1024,
    };
}

/// One bounded slice of a rehash.
#[derive(Debug)]
pub struct RehashChunk {
    /// What actually changed among this chunk's candidates.
    pub events: Vec<FileEvent>,
    /// Every candidate this chunk covered, with the version it was hashed
    /// against — including the ones that produced no event, so the caller can
    /// release exactly these reads (see `Registry::end_read`).
    pub baselines: HashMap<PathBuf, Version>,
    /// Whether the existence probes found a created or deleted tree entry. Set
    /// on the first chunk only; later chunks carry `false`.
    pub fs_structural: bool,
    /// Held until the chunk has been applied; the stream reads its next chunk
    /// only once this is dropped.
    pub ack: ChunkAck,
}

/// Lets one chunk out at a time: taken before a chunk is read, freed when that
/// chunk's [`ChunkAck`] is dropped.
#[derive(Default)]
struct Gate {
    out: Mutex<bool>,
    freed: Condvar,
}

impl Gate {
    /// Wait until no chunk is out, then let one out. Blocking.
    fn let_out(self: &Arc<Gate>) -> ChunkAck {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        while *out {
            out = self.freed.wait(out).unwrap_or_else(|e| e.into_inner());
        }
        *out = true;
        ChunkAck {
            _release: Arc::new(Release(Some(self.clone()))),
        }
    }
}

/// The app's acknowledgement that it has applied a [`RehashChunk`]: dropping
/// the last clone of it lets the stream read the next chunk. It rides in the
/// chunk's message (which must be `Clone`), so it is shared, and a message
/// that is dropped unapplied — stale, for a project already left — releases
/// it just the same.
#[derive(Clone)]
pub struct ChunkAck {
    // Never read: dropping it is the acknowledgement.
    _release: Arc<Release>,
}

impl ChunkAck {
    /// An acknowledgement nothing waits for, for a batch that did not come
    /// from [`rehash_stream`].
    #[cfg(test)]
    pub(crate) fn detached() -> ChunkAck {
        ChunkAck {
            _release: Arc::new(Release(None)),
        }
    }

    /// Gone (`upgrade` fails) once every clone of this ack is — for a test to
    /// see that a handler released it.
    #[cfg(test)]
    pub(crate) fn watch(&self) -> std::sync::Weak<impl Sized + use<>> {
        Arc::downgrade(&self._release)
    }
}

impl std::fmt::Debug for ChunkAck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChunkAck(..)")
    }
}

/// Frees the gate when the last [`ChunkAck`] clone goes.
struct Release(Option<Arc<Gate>>);

impl Drop for Release {
    fn drop(&mut self) {
        if let Some(gate) = self.0.take() {
            *gate.out.lock().unwrap_or_else(|e| e.into_inner()) = false;
            gate.freed.notify_all();
        }
    }
}

/// Split `candidates` into slices within `limits`, by count and by the sizes
/// the files have on disk right now (a `stat`, never a read).
fn plan_chunks(
    candidates: Vec<(PathBuf, Version)>,
    limits: ChunkLimits,
) -> Vec<Vec<(PathBuf, Version)>> {
    let mut chunks = Vec::new();
    let mut current: Vec<(PathBuf, Version)> = Vec::new();
    let mut bytes = 0u64;
    for candidate in candidates {
        let size = std::fs::symlink_metadata(&candidate.0)
            .map(|m| m.len())
            .unwrap_or(0);
        if !current.is_empty()
            && (current.len() >= limits.files.max(1) || bytes.saturating_add(size) > limits.bytes)
        {
            chunks.push(std::mem::take(&mut current));
            bytes = 0;
        }
        bytes = bytes.saturating_add(size);
        current.push(candidate);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// The blocking core of [`rehash_stream`]: probe for structural changes, then
/// rehash chunk by chunk — each read only once `let_out` lets it out (which
/// waits for the previous chunk's ack) — handing each to `sink`. `sink`
/// returning `false` (nobody is listening any more) stops the reading.
///
/// Every candidate lands in exactly one chunk's `baselines`, even when reading
/// its file panicked: a read the caller registered and never saw released would
/// block that slot for good.
fn rehash_chunks(
    root: &Path,
    candidates: Vec<(PathBuf, Version)>,
    probes: &[(PathBuf, bool)],
    max_bytes: u64,
    limits: ChunkLimits,
    mut let_out: impl FnMut() -> ChunkAck,
    mut sink: impl FnMut(RehashChunk) -> bool,
) {
    let mut fs_structural = structural_changes(probes);
    let chunks = plan_chunks(candidates, limits);
    if chunks.is_empty() {
        sink(RehashChunk {
            events: Vec::new(),
            baselines: HashMap::new(),
            fs_structural,
            ack: let_out(),
        });
        return;
    }
    for chunk in chunks {
        let ack = let_out();
        let baselines: HashMap<PathBuf, Version> = chunk.iter().cloned().collect();
        let events = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rehash(root, chunk, max_bytes)
        }))
        .unwrap_or_default();
        let delivered = sink(RehashChunk {
            events,
            baselines,
            fs_structural: std::mem::take(&mut fs_structural),
            ack,
        });
        if !delivered {
            return;
        }
    }
}

/// Rehash `candidates` (and run the existence `probes`) off the async threads,
/// as a stream of bounded chunks. A chunk is read only after the previous
/// one's [`ChunkAck`] was dropped, so one chunk's contents exist at a time
/// however large the batch — the consumer must drop each ack once it has used
/// the chunk, or the stream waits for it. Always yields at least one chunk
/// (carrying the structural verdict).
pub fn rehash_stream(
    root: PathBuf,
    candidates: Vec<(PathBuf, Version)>,
    probes: Probes,
    max_bytes: u64,
) -> impl Stream<Item = RehashChunk> {
    rehash_stream_with(root, candidates, probes, max_bytes, ChunkLimits::DEFAULT)
}

/// Changed paths to probe for a creation or a deletion (see
/// [`structural_changes`]), and the tree's file list to judge them against —
/// the project's own shared list, whose membership test (a set of every
/// path) is built by the producer, off the UI thread, and only when there is
/// a path to probe.
#[derive(Debug, Clone, Default)]
pub struct Probes {
    pub paths: Vec<PathBuf>,
    pub listed: Arc<Vec<clew_core::fs_scan::FileEntry>>,
}

impl Probes {
    /// Each path, paired with whether the tree lists it.
    fn decide(&self) -> Vec<(PathBuf, bool)> {
        if self.paths.is_empty() {
            return Vec::new();
        }
        let listed: std::collections::HashSet<&Path> =
            self.listed.iter().map(|f| f.abs.as_path()).collect();
        self.paths
            .iter()
            .map(|p| (p.clone(), listed.contains(p.as_path())))
            .collect()
    }
}

fn rehash_stream_with(
    root: PathBuf,
    candidates: Vec<(PathBuf, Version)>,
    probes: Probes,
    max_bytes: u64,
    limits: ChunkLimits,
) -> impl Stream<Item = RehashChunk> {
    iced::stream::channel(
        1,
        move |mut output: iced::futures::channel::mpsc::Sender<RehashChunk>| async move {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<RehashChunk>(1);
            let gate = Arc::new(Gate::default());
            let producer = tokio::task::spawn_blocking(move || {
                rehash_chunks(
                    &root,
                    candidates,
                    &probes.decide(),
                    max_bytes,
                    limits,
                    || gate.let_out(),
                    |chunk| tx.blocking_send(chunk).is_ok(),
                );
            });
            while let Some(chunk) = rx.recv().await {
                if output.send(chunk).await.is_err() {
                    break;
                }
            }
            // Dropping the receiver above ends a producer that is still
            // reading (a chunk it holds or waits for an ack on is dropped with
            // the channel, which frees the gate); wait for it so no read
            // outlives the stream.
            drop(rx);
            let _ = producer.await;
        },
    )
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
    use std::time::Duration;

    #[test]
    fn rehash_classifies_unchanged_modified_and_deleted() {
        let dir = clew_core::testutil::TempDir::new("watch-test");
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
        let dir = clew_core::testutil::TempDir::new("watch-swap-test");
        let outside_dir = clew_core::testutil::TempDir::new("watch-swap-outside");
        let outside = outside_dir.join("outside.txt");
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
            std::process::Command::new("/usr/bin/mkfifo")
                .arg(&pipe)
                .status()
                .is_ok_and(|s| s.success()),
            "mkfifo is needed for this test"
        );
        // On a thread, so a regression shows up as a failed assert rather than
        // a test run that never finishes.
        let (tx, rx) = std::sync::mpsc::channel();
        let (d, p) = (dir.to_path_buf(), pipe.clone());
        std::thread::spawn(move || {
            let _ = tx.send(rehash(&d, vec![(p, tracked)], CAP));
        });
        let out = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("rehash blocked opening the FIFO");
        assert!(matches!(&out[..], [FileEvent::Deleted(p)] if p == &pipe));
    }

    /// A file past the cap is dropped, not read: the refresh pipeline never
    /// sees it, and nothing downstream gets a chance to hold it in memory.
    #[test]
    fn rehash_drops_files_over_the_cap() {
        let dir = clew_core::testutil::TempDir::new("watch-cap-test");
        let big = dir.join("big.txt");
        std::fs::write(&big, vec![b'a'; 4096]).unwrap();

        assert!(rehash(&dir, vec![(big.clone(), 0)], 1024).is_empty());
        // The same file is reported normally once the cap allows it.
        assert!(matches!(
            &rehash(&dir, vec![(big.clone(), 0)], 8192)[..],
            [FileEvent::Modified(_)]
        ));
    }

    /// A large batch is read in bounded chunks: every candidate in exactly one
    /// chunk, no chunk over its limits, the structural verdict on the first.
    #[test]
    fn a_large_batch_is_rehashed_in_bounded_chunks() {
        let dir = clew_core::testutil::TempDir::new("watch-chunks");
        let mut candidates = Vec::new();
        for i in 0..10 {
            let f = dir.join(format!("f{i}.txt"));
            std::fs::write(&f, vec![b'a' + i as u8; 100]).unwrap();
            candidates.push((f, 0));
        }
        let created = dir.join("new.txt");
        std::fs::write(&created, "x").unwrap();
        let limits = ChunkLimits {
            files: 3,
            bytes: 250,
        };
        let mut chunks = Vec::new();
        rehash_chunks(
            &dir,
            candidates.clone(),
            &[(created, false)],
            1024,
            limits,
            ChunkAck::detached,
            |c| {
                chunks.push(c);
                true
            },
        );
        assert!(
            chunks.len() >= 5,
            "250 bytes fit two 100-byte files: {}",
            chunks.len()
        );
        for c in &chunks {
            assert!(c.baselines.len() <= 2 && !c.baselines.is_empty());
            assert_eq!(c.events.len(), c.baselines.len(), "all ten changed");
        }
        let covered: usize = chunks.iter().map(|c| c.baselines.len()).sum();
        assert_eq!(covered, candidates.len(), "every candidate exactly once");
        assert!(chunks[0].fs_structural, "the verdict rides the first chunk");
        assert!(chunks[1..].iter().all(|c| !c.fs_structural));

        // A consumer that goes away stops the reading.
        let mut taken = 0;
        rehash_chunks(
            &dir,
            candidates,
            &[],
            1024,
            limits,
            ChunkAck::detached,
            |_| {
                taken += 1;
                false
            },
        );
        assert_eq!(taken, 1);
    }

    /// With nothing to read there is still one chunk: the structural verdict.
    #[test]
    fn an_all_probe_batch_still_yields_its_verdict() {
        let dir = clew_core::testutil::TempDir::new("watch-probe");
        let gone = dir.join("gone.txt");
        let mut chunks = Vec::new();
        rehash_chunks(
            &dir,
            Vec::new(),
            &[(gone, true)],
            1024,
            ChunkLimits::DEFAULT,
            ChunkAck::detached,
            |c| {
                chunks.push(c);
                true
            },
        );
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].fs_structural && chunks[0].events.is_empty());
    }

    /// The stream delivers every chunk, in order, off the async threads, to a
    /// consumer that applies each one (drops its ack) before asking for the
    /// next.
    #[tokio::test]
    async fn the_stream_delivers_every_chunk() {
        use iced::futures::StreamExt;
        let dir = clew_core::testutil::TempDir::new("watch-stream");
        let candidates: Vec<_> = (0..7)
            .map(|i| {
                let f = dir.join(format!("s{i}.txt"));
                std::fs::write(&f, format!("{i}")).unwrap();
                (f, 0)
            })
            .collect();
        let mut stream = std::pin::pin!(rehash_stream_with(
            dir.to_path_buf(),
            candidates,
            Probes::default(),
            1024,
            ChunkLimits {
                files: 2,
                bytes: u64::MAX,
            },
        ));
        let mut sizes = Vec::new();
        while let Some(chunk) = stream.next().await {
            sizes.push(chunk.events.len());
        }
        assert_eq!(sizes, [2, 2, 2, 1]);
    }

    /// The hard bound: the next chunk is not READ until the previous one's ack
    /// is dropped. While the first chunk is held unapplied the second neither
    /// arrives nor is read — a file of it changed meanwhile arrives with its
    /// new bytes, which a read ahead of the ack would have missed.
    #[tokio::test]
    async fn the_next_chunk_is_read_only_after_the_previous_one_is_applied() {
        use iced::futures::StreamExt;
        let dir = clew_core::testutil::TempDir::new("watch-ack");
        let first = dir.join("a.txt");
        let second = dir.join("b.txt");
        std::fs::write(&first, "a, as read").unwrap();
        std::fs::write(&second, "b, before").unwrap();
        let mut stream = std::pin::pin!(rehash_stream_with(
            dir.to_path_buf(),
            vec![(first.clone(), 0), (second.clone(), 0)],
            Probes::default(),
            1024,
            ChunkLimits {
                files: 1,
                bytes: u64::MAX,
            },
        ));
        let held = stream.next().await.expect("the first chunk");
        assert!(held.baselines.contains_key(&first));
        // Ample time for a reader that did not wait to read ahead and hand
        // the second chunk over.
        let early = tokio::time::timeout(Duration::from_millis(300), stream.next()).await;
        assert!(
            early.is_err(),
            "the second chunk arrived while the first was unapplied"
        );
        std::fs::write(&second, "b, after the first chunk was applied").unwrap();
        drop(held);
        let next = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("dropping the ack lets the next chunk through")
            .expect("the second chunk");
        match &next.events[..] {
            [FileEvent::Modified(c)] => {
                assert_eq!(c.path, second);
                assert_eq!(*c.content, "b, after the first chunk was applied");
            }
            other => panic!("expected the second file's new bytes, got {other:?}"),
        }
        drop(next);
        assert!(stream.next().await.is_none(), "two chunks in all");
    }

    #[test]
    fn structural_changes_flags_creates_and_deletes_only() {
        let dir = clew_core::testutil::TempDir::new("structural-test");
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
    }
}
