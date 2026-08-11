//! Safe read/write for clew's own state files.
//!
//! Project state lives under `<root>/.clew/`, which ships with the repository
//! — a hostile repo controls its initial content. Reads must therefore refuse
//! anything that is not a plain, reasonably-sized file: a symlink to
//! `/dev/zero` would hang the open, one to a huge file would OOM, and one to
//! a sensitive file would quietly pull its contents into state that clew
//! later re-saves. Writes must never follow a pre-planted symlink at any of
//! the three names they touch: the temp file, the destination, or the `.lock`
//! file beside them.
//!
//! Every `.clew/` (and global data dir) load/save goes through these two
//! functions, so the rules live in exactly one place.

use std::io::Write;
use std::path::Path;

/// Byte cap for one state file. Explanation caches on large projects reach
/// single-digit megabytes; this is far above any legitimate file and far
/// below what would hurt to read.
pub const MAX_STATE_BYTES: u64 = 64 * 1024 * 1024;

/// Read a state file as text. `None` when it is missing, is not a plain file
/// (symlink, FIFO, device — checked via `symlink_metadata`, which does not
/// follow links), or exceeds [`MAX_STATE_BYTES`].
pub fn read(path: &Path) -> Option<String> {
    read_capped(path, MAX_STATE_BYTES)
}

/// [`read`] with an explicit cap, for files that should be far smaller
/// (configs, indexes) — and for any repository-controlled file that must be
/// read without trusting its size or type (see [`crate::imports`]).
///
/// The cap is enforced on the READ, not only on the size the handle reported:
/// a file can grow between the `fstat` and the read (an appending process, a
/// pipe-like file), and a size check alone would let it past. The `fstat`
/// stays as a cheap early rejection so an oversized file is refused without
/// reading it first.
pub fn read_capped(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::Read;
    if !repo_dirs_are_real(path) {
        return None;
    }
    let f = open_plain(path)?;
    if f.metadata().ok()?.len() > max_bytes {
        return None;
    }
    let mut s = String::new();
    // `max_bytes + 1`: reading one byte past the cap is what distinguishes
    // "exactly at the limit" from "grew past it while we were reading".
    f.take(max_bytes + 1).read_to_string(&mut s).ok()?;
    if s.len() as u64 > max_bytes {
        return None;
    }
    Some(s)
}

/// Open `path` as a plain file, race-free: the leaf must not be a symlink
/// (`O_NOFOLLOW`), the open never blocks on a FIFO (`O_NONBLOCK`), and the
/// file-type check runs on the OPEN handle (fstat) — so nothing swapped in
/// between a check and the read can redirect or wedge it.
#[cfg(unix)]
pub fn open_plain(path: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    Some(f)
}

/// Best effort without O_NOFOLLOW: pre-check, then open. The residual
/// check-to-open race exists only on non-unix hosts.
#[cfg(not(unix))]
pub fn open_plain(path: &Path) -> Option<std::fs::File> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    std::fs::File::open(path).ok()
}

/// Delete a state file (the empty-state save path). A missing file is fine;
/// everything else that stops the delete is an ERROR the caller must see — a
/// swallowed failure leaves stale state that quietly resurrects on the next
/// launch. Refuses (like every state operation) when a `.clew` ancestor is a
/// symlink or the target is not a plain file: with `.clew -> /outside`, the
/// fixed file names clew deletes would land on someone else's files.
/// (`remove_file` itself never follows a symlink at the leaf.)
pub fn remove(path: &Path) -> std::io::Result<()> {
    if !repo_dirs_are_real(path) {
        return Err(std::io::Error::other(
            "a state directory is a symlink — refusing to delete through it",
        ));
    }
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_file() => std::fs::remove_file(path),
        Ok(_) => Err(std::io::Error::other(
            "refusing to delete: not a plain file",
        )),
    }
}

/// Every directory from the path's `.clew` component down to its parent must
/// be a real directory, not a symlink: `.clew` ships with the repository, so
/// `.clew -> /outside` (or `.clew/cache -> …`) would redirect every state
/// read, write and delete outside the project. Ancestors ABOVE `.clew` are
/// not checked — a symlinked project root is the user's own, legitimate path
/// choice. Paths with no `.clew` component (the global data dir) have no
/// repo-controlled segment and pass.
///
/// Missing directories pass: the write path creates them (as real
/// directories) right after this check.
///
/// Residual, deliberately accepted: this walks the chain by NAME, so a
/// directory replaced with a symlink between this check and the operation
/// that follows would not be caught. Closing that needs the whole read/write
/// path rebuilt on `openat`/`renameat` against held directory handles.
///
/// The threat this module defends against is a repository's *committed*
/// contents — a `.clew` symlink that is already there when clew opens the
/// project — and against that, checking by name is exact. Winning the
/// remaining window instead requires an attacker already executing code on
/// the user's machine, concurrently, at which point clew's state files are
/// not the interesting target. Every leaf is separately safe regardless:
/// reads open with `O_NOFOLLOW` and type-check the handle, writes create their
/// temp file with `O_EXCL` and `rename` over the destination rather than
/// writing through whatever is there, and [`lock_exclusive`]'s `.lock` file —
/// the one leaf a repository can plant a link at without also having to supply
/// its contents — opens with `O_NOFOLLOW` and type-checks its handle too.
pub(crate) fn repo_dirs_are_real(path: &Path) -> bool {
    use std::path::PathBuf;
    let comps: Vec<_> = path.components().collect();
    let Some(pos) = comps.iter().position(|c| c.as_os_str() == ".clew") else {
        return true;
    };
    let mut probe = PathBuf::new();
    for c in &comps[..pos] {
        probe.push(c);
    }
    // Probe each prefix from `.clew` (inclusive) up to the file's parent.
    for c in &comps[pos..comps.len().saturating_sub(1)] {
        probe.push(c);
        match std::fs::symlink_metadata(&probe) {
            Ok(m) if m.is_dir() => {}
            // Not there yet (fresh project): the rest is missing too, and
            // whoever creates it creates real directories.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            // A symlink, a file squatting on the name, or an unreadable
            // entry: refuse.
            _ => return false,
        }
    }
    true
}

/// Atomic write: a uniquely-named `create_new` temp file beside the target,
/// then rename over it.
///
/// - `create_new` (O_CREAT|O_EXCL) refuses to open through anything already
///   at the temp path — including a dangling symlink a repository planted at
///   a predictable name.
/// - `rename` replaces a symlink at the destination rather than writing
///   through it.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_mode(path, bytes, None)
}

/// [`write_atomic`] for files carrying secrets (API keys): the temp file is
/// CREATED readable only by the user (0600 on unix), so the secret never
/// exists on disk with wider permissions — not even between create and
/// rename. (The rename preserves the temp file's mode.)
pub fn write_atomic_secret(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_mode(path, bytes, Some(0o600))
}

fn write_atomic_mode(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("state path has no parent"))?;
    // Checked BEFORE create_dir_all: with `.clew -> /outside` already in the
    // repo, create_dir_all would follow the link and the whole write (temp
    // file included) would land outside the project.
    if !repo_dirs_are_real(path) {
        return Err(std::io::Error::other(
            "a state directory is a symlink — refusing to write through it",
        ));
    }
    std::fs::create_dir_all(dir)?;
    let base = path.file_name().unwrap_or_default().to_string_lossy();
    let pid = std::process::id();
    for attempt in 0..16u32 {
        let tmp = dir.join(format!(".{base}.{pid}.{attempt}.tmp"));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        match opts.open(&tmp) {
            Ok(mut f) => {
                let written = f.write_all(bytes).and_then(|_| f.flush());
                drop(f);
                if let Err(e) = written {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                let renamed = std::fs::rename(&tmp, path);
                if renamed.is_err() {
                    let _ = std::fs::remove_file(&tmp);
                }
                return renamed;
            }
            // Something (stale temp, planted link) occupies this name: next.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(
        "could not create a temp file for the atomic write",
    ))
}

/// Apply one entry-level change to a state file that holds a JSON array of
/// objects, returning the file's new text (`None` = the store is empty and its
/// file should be deleted, which is what every such store means by an empty
/// list).
///
/// This is the merge half of [`clew_protocol::StateMerge`]: the same
/// read-modify-write the local stores do under [`lock_exclusive`], expressed
/// as data so it can be carried over the wire and applied where the file is —
/// the only place two clients' writes are both visible.
///
/// An unparseable (or missing) file is treated as an empty array, matching
/// what the client-side `from_text` of every one of these stores does with the
/// same bytes. It means a hand-corrupted store is rewritten rather than
/// preserved, which is the behaviour that was already there.
pub fn merge_entries(current: Option<&str>, op: &clew_protocol::StateMerge) -> Option<String> {
    use clew_protocol::StateEdit;
    use serde_json::Value;

    let mut list: Vec<Value> = current
        .and_then(|t| serde_json::from_str::<Vec<Value>>(t).ok())
        .unwrap_or_default();
    let at = list.iter().position(|e| op.matches(e));

    match (&op.edit, at) {
        (StateEdit::Remove, Some(i)) => {
            list.remove(i);
        }
        (StateEdit::Remove, None) => {}
        (StateEdit::Upsert(entry), Some(i)) => list[i] = entry.clone(),
        (StateEdit::Upsert(entry), None) => list.push(entry.clone()),
        // Toggle resolves against the file, not against the caller's copy of
        // it: whether the bookmark is there is exactly what a stale snapshot
        // gets wrong.
        (StateEdit::Toggle(_), Some(i)) => {
            list.remove(i);
        }
        (StateEdit::Toggle(entry), None) => list.push(entry.clone()),
        (
            StateEdit::Patch {
                fields,
                insert,
                empty_when,
            },
            at,
        ) => {
            let i = match (at, insert) {
                (Some(i), _) => i,
                // Nothing to patch and no seed: the entry the caller meant is
                // gone (another client deleted it). Dropping the patch is
                // right — resurrecting it from a stale copy is not.
                (None, None) => return finish(list, op.delete_when_empty),
                (None, Some(seed)) => {
                    list.push(seed.clone());
                    list.len() - 1
                }
            };
            if let Some(obj) = list[i].as_object_mut() {
                for (k, v) in fields {
                    obj.insert(k.clone(), v.clone());
                }
            }
            // An entry whose every "carries information" field is blank is
            // dropped — how a reading note with neither the understood flag
            // nor any text says it no longer exists. The store names the
            // fields; the rule is not baked in here.
            if !empty_when.is_empty() && empty_when.iter().all(|k| is_blank(&list[i], k)) {
                list.remove(i);
            }
        }
    }
    finish(list, op.delete_when_empty)
}

fn finish(list: Vec<serde_json::Value>, delete_when_empty: bool) -> Option<String> {
    if list.is_empty() && delete_when_empty {
        return None;
    }
    serde_json::to_string_pretty(&list).ok()
}

/// Whether `entry.key` carries no information: absent, null, false, or an
/// empty/whitespace-only string.
fn is_blank(entry: &serde_json::Value, key: &str) -> bool {
    match entry.get(key) {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Bool(b)) => !b,
        Some(serde_json::Value::String(s)) => s.trim().is_empty(),
        Some(_) => false,
    }
}

/// An exclusive advisory lock on one state file, held across a
/// read-modify-write and released when dropped.
///
/// Taken on a sibling `.lock` file rather than on the state file itself,
/// because [`write_atomic`] replaces that inode: a lock held on the old one
/// would guard a file that no longer exists at the name.
pub struct FileLock {
    #[cfg(unix)]
    #[allow(dead_code)]
    file: std::fs::File,
}

/// Take [`FileLock`] for `path`, blocking until it is free.
///
/// Every store that merges (`bookmarks`, `notes`, the walkthrough library, the
/// derived caches, `trust.toml`, `connections.toml`) does
/// load → change → `write_atomic`. An in-process `Mutex` serializes that
/// across a clew process's windows, but nothing spanned two clew PROCESSES —
/// two launches of the app, or a release build beside a dev one — so both
/// could read the same list, each apply its own change, and the later `rename`
/// win. The file itself is never torn (the write is atomic); one entry just
/// disappears, unreported.
///
/// **Best effort by design.** `None` when the lock cannot be taken (a
/// read-only checkout, a filesystem without `flock`, a non-unix host); callers
/// proceed unlocked, because refusing to save the user's bookmark because a
/// lock file could not be created would be worse than the race it prevents. So
/// this narrows the window to nothing on ordinary local filesystems and leaves
/// it exactly as wide as before everywhere else — it is not a guarantee.
///
/// The lock file is created beside the state file, dot-prefixed like the
/// atomic write's temp files (`.bookmarks.json.lock`). Inside a project that
/// is `.clew/`, which the scanner prunes unconditionally, so it never shows up
/// as a project file — but it IS a new file in the user's directory, and a
/// repository that commits `.clew/` will see it as untracked. Its name is
/// therefore predictable to whoever wrote the repository, so the open refuses
/// to follow a symlink at it; otherwise the "new file" would be created
/// wherever a committed link pointed.
pub fn lock_exclusive(path: &Path) -> Option<FileLock> {
    // The same refusal every other state operation makes: with `.clew` (or
    // `.clew/cache`) shipped as a symlink, creating the lock file would land
    // outside the project.
    if !repo_dirs_are_real(path) {
        return None;
    }
    let dir = path.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    let name = path.file_name()?.to_string_lossy();
    let lock_path = dir.join(format!(".{name}.lock"));
    lock_file(&lock_path)
}

#[cfg(unix)]
fn lock_file(lock_path: &Path) -> Option<FileLock> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    // The lock file's name is fully determined by the state file's, and it
    // lives in `.clew/`, which ships with the repository — so a clone can
    // carry a symlink already sitting at exactly this name. Without
    // `O_NOFOLLOW` this open follows it and CREATES a file wherever it points,
    // which is precisely the unconsented write outside the project that the
    // rest of this module exists to prevent. `O_NONBLOCK` keeps a planted FIFO
    // from wedging the open (nothing ever opens the other end), and the type
    // check runs on the OPEN handle, so it describes what was actually locked.
    // Refusing here is not a failed save: `None` means the caller proceeds
    // unlocked, the degradation this function already documents.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    // Blocking, exclusive; released when the handle closes. The critical
    // section is one small read plus one rename.
    (unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0).then_some(FileLock { file })
}

/// No advisory locking here: the cross-process race stays open on non-unix
/// hosts, exactly as it was. The in-process `Mutex` each store holds is still
/// what covers the common (two windows, one process) case.
#[cfg(not(unix))]
fn lock_file(_lock_path: &Path) -> Option<FileLock> {
    Some(FileLock {})
}

/// Whether `rel` — a root-relative path from a persisted state file — is safe
/// to join onto the project root: relative, and made only of normal
/// components (no `..`, no leading `/`). State files ship with the repo, so a
/// stored path is attacker data until proven boring.
pub fn safe_rel(rel: &str) -> bool {
    let p = Path::new(rel);
    !rel.is_empty()
        && !p.is_absolute()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Whether an **absolute** path from a persisted state file really lies under
/// `root`. `starts_with` alone is lexical — `/root/../../etc/x` passes it —
/// so `..`/`.` components are rejected outright, and the resolved path is
/// re-checked: a repo-shipped `root/link -> /outside` makes `root/link/x`
/// pass every lexical test while reading someone else's file.
pub fn safe_abs_under(root: &Path, path: &Path) -> bool {
    use std::path::Component;
    let lexical = path.starts_with(root)
        && path.components().all(|c| {
            matches!(
                c,
                Component::Normal(_) | Component::RootDir | Component::Prefix(_)
            )
        });
    if !lexical {
        return false;
    }
    // Containment must survive symlink resolution. A path that cannot be
    // resolved (missing file, dangling link) passes — it cannot be read
    // either, and refusing it would break "file was deleted" flows.
    match (path.canonicalize(), root.canonicalize()) {
        (Ok(p), Ok(r)) => p.starts_with(&r),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn read_refuses_symlinks_and_oversize() {
        let d = dir("clew-statefile-read");
        std::fs::write(d.join("ok.json"), "{}").unwrap();
        assert_eq!(read(&d.join("ok.json")).as_deref(), Some("{}"));
        assert!(read(&d.join("missing.json")).is_none());
        // A symlink — even to a readable file — is refused: state files are
        // plain files, and a repo-shipped link is how the DoS/leak starts.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.join("ok.json"), d.join("link.json")).unwrap();
            assert!(read(&d.join("link.json")).is_none());
            std::os::unix::fs::symlink("/dev/zero", d.join("zero.json")).unwrap();
            assert!(read(&d.join("zero.json")).is_none());
        }
        // Over the cap: refused without reading.
        std::fs::write(d.join("big.json"), "x").unwrap();
        assert!(read_capped(&d.join("big.json"), 0).is_none());
    }

    /// The cap binds the READ, not just the size the handle reported. A file
    /// that is within the cap at `fstat` time and grows past it before the
    /// read completes must still be refused — otherwise the size check is
    /// only advisory, and an appending writer defeats it.
    #[test]
    fn read_cap_survives_a_file_that_grows_after_the_size_check() {
        let d = dir("clew-statefile-grow");
        let path = d.join("grow.json");
        std::fs::write(&path, "x".repeat(64)).unwrap();
        // Well within the cap: read normally.
        assert_eq!(read_capped(&path, 128).map(|s| s.len()), Some(64));
        // At exactly the cap: still fine (the +1 probe must not false-trip).
        assert_eq!(read_capped(&path, 64).map(|s| s.len()), Some(64));
        // One byte over: refused.
        assert!(read_capped(&path, 63).is_none());
    }

    #[test]
    fn write_atomic_defeats_planted_links() {
        let d = dir("clew-statefile-write");
        let target = d.join("state.json");
        // A symlink planted AT the destination is replaced, not followed:
        // the outside file must remain untouched.
        let outside = d.join("outside.txt");
        std::fs::write(&outside, "precious").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &target).unwrap();
        write_atomic(&target, b"{\"v\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"v\":1}");
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "precious",
            "a planted destination symlink must not be written through"
        );
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(&target).unwrap().is_file(),
            "the destination is a plain file now"
        );
        // Overwrite works (temp names don't collide with the previous run).
        write_atomic(&target, b"{\"v\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"v\":2}");
    }

    #[test]
    fn safe_abs_under_rejects_lexical_tricks() {
        let root = Path::new("/proj");
        assert!(safe_abs_under(root, Path::new("/proj/src/a.rs")));
        assert!(!safe_abs_under(root, Path::new("/etc/passwd")));
        // Lexically "under" the root, actually outside it.
        assert!(!safe_abs_under(root, Path::new("/proj/../../etc/passwd")));
        assert!(!safe_abs_under(root, Path::new("/proj/./x/../../../etc")));
    }

    #[test]
    fn safe_rel_rejects_escapes() {
        assert!(safe_rel("src/lib.rs"));
        assert!(safe_rel("a/b/c.txt"));
        assert!(!safe_rel(""));
        assert!(!safe_rel("/etc/passwd"));
        assert!(!safe_rel("../outside.rs"));
        assert!(!safe_rel("a/../../outside.rs"));
        assert!(!safe_rel("./a.rs")); // CurDir is not a Normal component
    }

    /// `.clew` itself being a symlink must stop every state operation: read,
    /// write (including its temp file), and the empty-state delete would all
    /// land outside the project otherwise.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_clew_dir_stops_reads_writes_and_deletes() {
        let d = dir("clew-statefile-linkdir");
        let root = d.join("proj");
        let outside = d.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("notes.json"), "[1]").unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".clew")).unwrap();

        let state = root.join(".clew").join("notes.json");
        assert!(
            read(&state).is_none(),
            "reading through a linked .clew must refuse"
        );
        assert!(
            write_atomic(&state, b"[2]").is_err(),
            "writing through a linked .clew must refuse"
        );
        assert!(
            remove(&state).is_err(),
            "deleting through a linked .clew must refuse LOUDLY"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("notes.json")).unwrap(),
            "[1]",
            "the outside file survives untouched"
        );

        // A nested link (`.clew/cache -> outside`) under a real .clew is
        // refused the same way.
        let root2 = d.join("proj2");
        std::fs::create_dir_all(root2.join(".clew")).unwrap();
        std::os::unix::fs::symlink(&outside, root2.join(".clew").join("cache")).unwrap();
        let nested = root2.join(".clew").join("cache").join("stats.json");
        assert!(write_atomic(&nested, b"{}").is_err());
        assert!(read(&nested).is_none());

        // A real .clew (even one that does not exist yet) keeps working.
        let root3 = d.join("proj3");
        std::fs::create_dir_all(&root3).unwrap();
        let fresh = root3.join(".clew").join("notes.json");
        write_atomic(&fresh, b"[3]").unwrap();
        assert_eq!(read(&fresh).as_deref(), Some("[3]"));
        remove(&fresh).unwrap();
        assert!(read(&fresh).is_none());
        // Deleting a file that is already gone is not an error.
        remove(&fresh).unwrap();
    }

    /// The lock has to be an OS-level one, not a process-local mutex: a second
    /// clew process is exactly the writer the in-process locks cannot see.
    ///
    /// `flock` is held per OPEN FILE DESCRIPTION, so two independent
    /// acquisitions contend the same way whether they come from two threads or
    /// two processes — which is what makes this testable in-process at all.
    /// Without the lock the second acquisition returns immediately and the
    /// read-modify-writes interleave.
    #[test]
    #[cfg(unix)]
    fn a_held_lock_blocks_the_next_acquisition() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let d = dir("clew-statefile-lock");
        let path = d.join(".clew").join("bookmarks.json");

        let held = lock_exclusive(&path).expect("the lock is available");
        let entered = AtomicBool::new(false);
        std::thread::scope(|s| {
            let waiter = s.spawn(|| {
                let _second = lock_exclusive(&path).expect("acquired once we let go");
                entered.store(true, Ordering::SeqCst);
            });
            std::thread::sleep(std::time::Duration::from_millis(80));
            assert!(
                !entered.load(Ordering::SeqCst),
                "a second holder must wait while the first is inside its \
                 read-modify-write"
            );
            drop(held);
            waiter.join().unwrap();
        });
        assert!(entered.load(Ordering::SeqCst));
        // The lock file lives beside the store, dot-prefixed like the atomic
        // write's temp files, and never replaces the store itself.
        assert!(d.join(".clew").join(".bookmarks.json.lock").is_file());
        assert!(!path.exists());
    }

    /// A symlinked `.clew` stops the lock too: creating the lock file through
    /// it would put clew's file in someone else's directory.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_clew_dir_stops_the_lock() {
        let d = dir("clew-statefile-lock-link");
        let root = d.join("proj");
        let outside = d.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".clew")).unwrap();
        assert!(lock_exclusive(&root.join(".clew").join("notes.json")).is_none());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    /// The lock file is the third leaf a state write touches, and the only one
    /// whose name a repository can plant a link at without also supplying the
    /// file's contents: `.clew/` ships with the repo and `.bookmarks.json.lock`
    /// is fully determined by `bookmarks.json`. Following that link would
    /// create a file at the attacker's target — an unconsented write outside
    /// the project, from nothing but a clone and one bookmark keypress.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_lock_file_is_refused_and_creates_nothing_outside() {
        let d = dir("clew-statefile-lock-leaf");
        let root = d.join("proj");
        let clew = root.join(".clew");
        std::fs::create_dir_all(&clew).unwrap();
        let outside = d.join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        // Dangling on purpose: the damage is the CREATE, not an overwrite.
        let target = outside.join("planted");
        std::os::unix::fs::symlink(&target, clew.join(".bookmarks.json.lock")).unwrap();

        assert!(
            lock_exclusive(&clew.join("bookmarks.json")).is_none(),
            "a symlink at the lock file's name must refuse the lock, not be \
             followed"
        );
        assert!(
            !target.exists(),
            "nothing may be created at the link's target"
        );
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "the outside directory stays empty"
        );

        // A FIFO squatting the same name is refused too, and must not block
        // the open — the lock is taken on the iced update thread.
        let fifo_root = d.join("proj-fifo");
        let fifo_clew = fifo_root.join(".clew");
        std::fs::create_dir_all(&fifo_clew).unwrap();
        let fifo = fifo_clew.join(".notes.json.lock");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        assert!(
            lock_exclusive(&fifo_clew.join("notes.json")).is_none(),
            "a FIFO at the lock file's name must be refused, not locked"
        );

        // The ordinary case still works: a real lock file is created and held.
        let ok_root = d.join("proj-ok");
        std::fs::create_dir_all(ok_root.join(".clew")).unwrap();
        assert!(lock_exclusive(&ok_root.join(".clew").join("bookmarks.json")).is_some());
    }

    fn merge(edit: clew_protocol::StateEdit, line: i64) -> clew_protocol::StateMerge {
        clew_protocol::StateMerge {
            key_fields: vec!["rel".into(), "line".into()],
            key: vec!["a.rs".into(), line.into()],
            edit,
            delete_when_empty: true,
        }
    }

    /// The merge addresses ONE entry, so everything else in the file — every
    /// bookmark another client added since this one loaded it — survives.
    #[test]
    fn merge_touches_only_the_entry_it_addresses() {
        use clew_protocol::StateEdit;
        let theirs = r#"[{"rel":"z.rs","line":9,"preview":"theirs"}]"#;

        // Toggle resolves against the FILE: absent here, so it is an add.
        let added = merge_entries(
            Some(theirs),
            &merge(
                StateEdit::Toggle(serde_json::json!({"rel":"a.rs","line":1,"preview":"mine"})),
                1,
            ),
        )
        .expect("not empty");
        let list: Vec<serde_json::Value> = serde_json::from_str(&added).unwrap();
        assert_eq!(list.len(), 2, "the other client's bookmark survives");
        assert_eq!(list[1]["preview"], "mine");

        // Present now, so the same toggle removes it — and only it.
        let removed = merge_entries(
            Some(&added),
            &merge(
                StateEdit::Toggle(serde_json::json!({"rel":"a.rs","line":1,"preview":"mine"})),
                1,
            ),
        )
        .expect("not empty");
        let list: Vec<serde_json::Value> = serde_json::from_str(&removed).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["rel"], "z.rs");

        // Remove by identity is a no-op for an entry that is already gone.
        let same = merge_entries(Some(&removed), &merge(StateEdit::Remove, 1)).expect("not empty");
        assert_eq!(
            serde_json::from_str::<Vec<serde_json::Value>>(&same)
                .unwrap()
                .len(),
            1
        );
    }

    /// Patch merges fields into the entry ON DISK, so a field another client
    /// wrote is not overwritten by a stale copy of the whole entry; and an
    /// entry left carrying nothing is dropped, the rule the store names.
    #[test]
    fn patch_keeps_the_fields_it_was_not_given() {
        use clew_protocol::StateEdit;
        let on_disk = r#"[{"rel":"a.rs","symbol":"f","understood":true,"text":"theirs"}]"#;
        let note_merge = |edit| clew_protocol::StateMerge {
            key_fields: vec!["rel".into(), "symbol".into()],
            key: vec!["a.rs".into(), "f".into()],
            edit,
            delete_when_empty: true,
        };
        let empty_when = vec!["understood".to_string(), "text".to_string()];

        let mut fields = serde_json::Map::new();
        fields.insert("understood".into(), false.into());
        let patched = merge_entries(
            Some(on_disk),
            &note_merge(StateEdit::Patch {
                fields,
                insert: None,
                empty_when: empty_when.clone(),
            }),
        )
        .expect("the note still has text");
        let list: Vec<serde_json::Value> = serde_json::from_str(&patched).unwrap();
        assert_eq!(
            list[0]["text"], "theirs",
            "prose another client wrote must survive a flag change"
        );
        assert_eq!(list[0]["understood"], false);

        // Clearing the last field that carried information drops the entry,
        // and an emptied store asks to be deleted.
        let mut fields = serde_json::Map::new();
        fields.insert("text".into(), "  ".into());
        assert!(
            merge_entries(
                Some(&patched),
                &note_merge(StateEdit::Patch {
                    fields,
                    insert: None,
                    empty_when,
                }),
            )
            .is_none()
        );
    }

    /// A patch with no seed must not resurrect an entry another client
    /// deleted: attaching a note to a bookmark that is gone is a no-op.
    #[test]
    fn patch_without_a_seed_does_not_recreate_a_deleted_entry() {
        let mut fields = serde_json::Map::new();
        fields.insert("note".into(), "typed prose".into());
        let out = merge_entries(
            Some("[]"),
            &merge(
                clew_protocol::StateEdit::Patch {
                    fields,
                    insert: None,
                    empty_when: Vec::new(),
                },
                1,
            ),
        );
        assert!(out.is_none(), "nothing to patch, nothing written");
    }

    /// Lexical containment is not containment: a repo-shipped symlink inside
    /// the root reaches outside while every component looks normal.
    #[test]
    #[cfg(unix)]
    fn safe_abs_under_rejects_symlink_escapes() {
        let d = dir("clew-statefile-abs-link");
        let root = d.join("proj");
        let outside = d.join("outside");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}").unwrap();
        std::fs::write(outside.join("secret.txt"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        assert!(safe_abs_under(&root, &root.join("src/a.rs")));
        assert!(
            !safe_abs_under(&root, &root.join("link").join("secret.txt")),
            "a symlink inside the root must not smuggle outside files in"
        );
        // A missing path stays (lexically) allowed: it cannot be read anyway.
        assert!(safe_abs_under(&root, &root.join("src/deleted.rs")));
    }
}
