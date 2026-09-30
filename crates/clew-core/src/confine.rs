//! Path confinement: resolve a project-relative path that arrived from outside
//! (a client request, a model-written tool argument) against the project root,
//! refusing anything that would land outside it.
//!
//! This is the ONE implementation of that predicate. The server's request
//! handlers (`ReadFile`, `GitInfo`, and the path arguments of `Git`,
//! lexically) and the Ask agent's tools used to carry their own copies, and
//! they had already drifted: only one of them rejected `RootDir`/`Prefix`
//! components. A security predicate kept in two places is two chances for one
//! of them to drift open.
//!
//! `ReadSources` takes the lexical stage from here and then reads each file
//! as the client reads one of its own project, through
//! `fs_scan::read_confined_capped_checked` — whose containment check is the
//! client's own — so that both sides say the same of a link: a host that
//! resolved the path first read what a link at it led to, where the client
//! refused the link.
//!
//! Two stages, because they cost different things:
//!
//! - [`check_lexical`] looks at the string only — absolute paths, `..`, root
//!   and drive-prefix components, empty input. No I/O, so a request handler may
//!   run it inline and refuse at once.
//! - [`confine`] does that and then canonicalizes both the root and the joined
//!   path, which resolves every symlink on the way: a link inside the project
//!   that points outside it is refused too. That is filesystem I/O (it can
//!   stall on a slow or network mount), so async callers run it on a blocking
//!   thread (`spawn_blocking`), never on the reactor.
//!
//! What this does NOT close: the canonical path is checked, then used by name
//! afterwards, so a directory swapped for a symlink in between is not caught.
//! Callers that read open the leaf with `O_NOFOLLOW` and type-check the handle
//! (see `statefile::open_plain`). Those that open the name they were asked for
//! rather than the canonical path — so that a link at that name is refused —
//! also follow a folder link on the way as it stands when they open: one
//! re-pointed after the check is followed where it then leads. Either
//! directory-swap window needs an attacker already running code on the host,
//! which is out of scope here (the same residual `statefile` documents).

use std::path::{Component, Path, PathBuf};

/// Why a path was refused.
#[derive(Debug)]
pub enum ConfineError {
    /// The path is empty (or only whitespace): it names no file.
    Empty,
    /// The path is absolute, or carries a root / drive-prefix component.
    Absolute,
    /// The path has a `..` component. Refused even when it would resolve back
    /// inside the root: nothing legitimate needs it, and allowing it would make
    /// the lexical stage depend on the filesystem.
    Traversal,
    /// The project root itself cannot be resolved.
    Root(std::io::Error),
    /// The target does not exist, or cannot be resolved (permission, a
    /// dangling link, an invalid name).
    Unresolvable(std::io::Error),
    /// The target resolves outside the root — through a symlink somewhere on
    /// its path.
    Escapes,
}

impl std::fmt::Display for ConfineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfineError::Empty => f.write_str("empty path"),
            ConfineError::Absolute => f.write_str("absolute path"),
            ConfineError::Traversal => f.write_str("path contains `..`"),
            ConfineError::Root(e) => write!(f, "project root unavailable: {e}"),
            ConfineError::Unresolvable(e) => write!(f, "cannot resolve path: {e}"),
            ConfineError::Escapes => f.write_str("path resolves outside the project"),
        }
    }
}

impl std::error::Error for ConfineError {}

impl ConfineError {
    /// Whether the refusal is about the path's SHAPE (decided without I/O),
    /// as opposed to what the filesystem holds. Shape refusals are always an
    /// escape attempt or a malformed request; the others can be an ordinary
    /// missing file.
    pub fn is_escape(&self) -> bool {
        matches!(
            self,
            ConfineError::Empty
                | ConfineError::Absolute
                | ConfineError::Traversal
                | ConfineError::Escapes
        )
    }
}

/// The lexical half of [`confine`]: refuse a relative path whose shape alone
/// could leave the root. Pure — no filesystem access — so it is safe to call
/// on an async thread.
pub fn check_lexical(rel: &str) -> Result<(), ConfineError> {
    if rel.trim().is_empty() {
        return Err(ConfineError::Empty);
    }
    let path = Path::new(rel);
    if path.is_absolute() {
        return Err(ConfineError::Absolute);
    }
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => return Err(ConfineError::Absolute),
            Component::ParentDir => return Err(ConfineError::Traversal),
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

/// Resolve `rel` against `root`, refusing anything that escapes the project.
///
/// Returns the CANONICAL absolute path — symlinks resolved — only when it
/// genuinely lives inside the canonical root. Canonical on purpose: external
/// processes (language servers, git) report canonical paths, so downstream
/// consumers agree with them even when the root itself is reached through a
/// symlink (the system temp dir on macOS is one).
///
/// Blocking (canonicalization is filesystem I/O): async callers run this on a
/// blocking thread.
pub fn confine(root: &Path, rel: &str) -> Result<PathBuf, ConfineError> {
    check_lexical(rel)?;
    let canonical_root = std::fs::canonicalize(root).map_err(ConfineError::Root)?;
    // A name ending in `/` or `/.` names a folder, as POSIX resolves it:
    // Linux's `realpath` refuses one that leads to anything else, while
    // macOS's took `alias.rs/` for the file a link there leads to — a
    // spelling that got past a caller's refusal of a link at the name,
    // whose `lstat` of it goes through the link. Refused here on both, and
    // in the same words: the OS's own are the host's — before containment,
    // which Linux never reaches for such a name, so that one led out of the
    // project is refused alike too.
    let folder = names_a_folder(rel);
    let not_a_folder = || ConfineError::Unresolvable(std::io::ErrorKind::NotADirectory.into());
    let canonical = std::fs::canonicalize(canonical_root.join(rel)).map_err(|e| {
        if folder && e.kind() == std::io::ErrorKind::NotADirectory {
            not_a_folder()
        } else {
            ConfineError::Unresolvable(e)
        }
    })?;
    if folder && !canonical.is_dir() {
        return Err(not_a_folder());
    }
    if !canonical.starts_with(&canonical_root) {
        return Err(ConfineError::Escapes);
    }
    Ok(canonical)
}

/// Whether `rel` ends in a separator or a `.` component (`src/`, `src/.`,
/// `.`): a name only a folder answers to, whatever the name before it is.
fn names_a_folder(rel: &str) -> bool {
    matches!(rel.rsplit('/').next(), Some("" | "."))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testutil::TempDir;

    fn project(tag: &str) -> TempDir {
        let dir = TempDir::new(tag);
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn f() {}\n").unwrap();
        dir
    }

    #[test]
    fn files_inside_the_root_resolve_to_their_canonical_path() {
        let dir = project("inside");
        let got = confine(dir.path(), "src/lib.rs").expect("inside the root");
        assert_eq!(got, dir.path().canonicalize().unwrap().join("src/lib.rs"));
        // `.` components are harmless.
        assert!(confine(dir.path(), "./src/./lib.rs").is_ok());
    }

    #[test]
    fn shapes_that_could_escape_are_refused_without_io() {
        for (rel, want) in [
            ("", "empty"),
            ("   ", "empty"),
            ("/etc/passwd", "absolute"),
            ("../outside.rs", "traversal"),
            ("src/../../outside.rs", "traversal"),
            // Refused even though it would land back inside: the lexical stage
            // must not depend on what the filesystem holds.
            ("src/../src/lib.rs", "traversal"),
        ] {
            let err = check_lexical(rel).expect_err(rel);
            let got = match err {
                ConfineError::Empty => "empty",
                ConfineError::Absolute => "absolute",
                ConfineError::Traversal => "traversal",
                other => panic!("{rel:?}: unexpected {other:?}"),
            };
            assert_eq!(got, want, "{rel:?}");
            assert!(err_is_escape(rel), "{rel:?} is an escape-shaped refusal");
        }
    }

    fn err_is_escape(rel: &str) -> bool {
        check_lexical(rel).is_err_and(|e| e.is_escape())
    }

    #[test]
    fn a_missing_file_is_unresolvable_not_an_escape() {
        let dir = project("missing");
        let err = confine(dir.path(), "src/nope.rs").expect_err("missing");
        assert!(matches!(err, ConfineError::Unresolvable(_)), "{err:?}");
        assert!(!err.is_escape());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_outside_the_root_is_refused() {
        let dir = project("symlink-out");
        let outside = TempDir::new("symlink-target");
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        // A file link and a directory link, both leaving the project.
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("leak.txt"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("src/vendor")).unwrap();
        for rel in ["leak.txt", "src/vendor/secret.txt"] {
            let err = confine(dir.path(), rel).expect_err(rel);
            assert!(matches!(err, ConfineError::Escapes), "{rel}: {err:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_staying_inside_the_root_is_allowed() {
        let dir = project("symlink-in");
        std::os::unix::fs::symlink(dir.path().join("src/lib.rs"), dir.path().join("alias.rs"))
            .unwrap();
        let got = confine(dir.path(), "alias.rs").expect("resolves inside");
        assert_eq!(got, dir.path().canonicalize().unwrap().join("src/lib.rs"));
    }

    /// A name ending in `/` or `/.` names a folder — through a link on the
    /// way, too — and a file is none: POSIX, and Linux's `realpath`, say so,
    /// while macOS's took `alias.rs/` for the file a link there leads to.
    /// That spelling got a link at the name past callers that refuse one
    /// (the Ask agent's `link_at`), whose `lstat` of it went through the
    /// link.
    #[cfg(unix)]
    #[test]
    fn a_name_ending_as_a_folder_resolves_only_to_a_folder() {
        let dir = project("folder-names");
        std::os::unix::fs::symlink(dir.path().join("src/lib.rs"), dir.path().join("alias.rs"))
            .unwrap();
        std::os::unix::fs::symlink(dir.path().join("src"), dir.path().join("linked")).unwrap();
        let outside = TempDir::new("folder-names-outside");
        std::fs::write(outside.join("secret.txt"), "secret\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.path().join("leak.txt"))
            .unwrap();
        // Said alike wherever the refusal comes from — this check on macOS,
        // `realpath` itself on Linux, and on both for a name that runs
        // through a file (`src/lib.rs/x/`): the OS's own words differ by
        // host.
        for rel in [
            "src/lib.rs/",
            "src/lib.rs/.",
            "alias.rs/",
            "alias.rs//",
            "alias.rs/.",
            "src/lib.rs/x/",
            "leak.txt/",
            "leak.txt/.",
        ] {
            match confine(dir.path(), rel) {
                Err(e @ ConfineError::Unresolvable(_)) => {
                    assert_eq!(
                        e.to_string(),
                        "cannot resolve path: not a directory",
                        "{rel}"
                    );
                }
                other => panic!("{rel}: {other:?}"),
            }
        }
        let canonical = dir.path().canonicalize().unwrap();
        for rel in ["src/", "src/.", "linked/", "linked/.", "."] {
            let want = if rel == "." {
                canonical.clone()
            } else {
                canonical.join("src")
            };
            assert_eq!(confine(dir.path(), rel).expect(rel), want, "{rel}");
        }
    }

    /// The root itself may be reached through a symlink (the macOS temp dir
    /// is one): containment is judged between CANONICAL paths, so a legitimate
    /// file under a symlinked root still resolves.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_still_confines_correctly() {
        let dir = project("symlinked-root");
        let link_parent = TempDir::new("root-link");
        let link = link_parent.join("proj");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        assert!(confine(&link, "src/lib.rs").is_ok());
        assert!(matches!(
            confine(&link, "../proj/src/lib.rs"),
            Err(ConfineError::Traversal)
        ));
    }

    #[test]
    fn an_unresolvable_root_is_reported_as_such() {
        let base = TempDir::new("confine-no-root");
        let missing = base.join("does-not-exist");
        assert!(matches!(
            confine(&missing, "a.rs"),
            Err(ConfineError::Root(_))
        ));
    }
}
