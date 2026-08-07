//! Safe read/write for clew's own state files.
//!
//! Project state lives under `<root>/.clew/`, which ships with the repository
//! — a hostile repo controls its initial content. Reads must therefore refuse
//! anything that is not a plain, reasonably-sized file: a symlink to
//! `/dev/zero` would hang the open, one to a huge file would OOM, and one to
//! a sensitive file would quietly pull its contents into state that clew
//! later re-saves. Writes must never follow a pre-planted symlink at either
//! the temp path or the destination.
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
/// not the interesting target. The leaf is separately safe regardless: reads
/// open with `O_NOFOLLOW` and type-check the handle, and writes create their
/// temp file with `O_EXCL` and `rename` over the destination rather than
/// writing through whatever is there.
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
