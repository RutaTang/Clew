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
/// (configs, indexes).
pub fn read_capped(path: &Path, max_bytes: u64) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > max_bytes {
        return None;
    }
    std::fs::read_to_string(path).ok()
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
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("state path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let base = path.file_name().unwrap_or_default().to_string_lossy();
    let pid = std::process::id();
    for attempt in 0..16u32 {
        let tmp = dir.join(format!(".{base}.{pid}.{attempt}.tmp"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
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
/// so `..`/`.` components are rejected outright.
pub fn safe_abs_under(root: &Path, path: &Path) -> bool {
    use std::path::Component;
    path.starts_with(root)
        && path.components().all(|c| {
            matches!(
                c,
                Component::Normal(_) | Component::RootDir | Component::Prefix(_)
            )
        })
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
}
