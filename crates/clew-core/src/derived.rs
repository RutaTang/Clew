//! Where clew keeps the artifacts it DERIVES from a project — the symbol
//! index, explanations, embeddings, the overview, statistics, rendered SVGs.
//!
//! Never inside the project. These used to live in `<root>/.clew/cache/`,
//! which the repository controls: every entry is keyed by a content hash the
//! repository can compute for its own files, so a committed cache is accepted
//! as clew's own work. That turns a checkout into a way to forge the symbol
//! index (and with it navigation, the graphs, the overview, and the source
//! clew hands to the model) with no code execution at all.
//!
//! The fix is not to authenticate the bytes but to stop the repository from
//! having a say: derived artifacts move to clew's own data directory, keyed
//! by the project. That also makes a read-only checkout cache normally.
//!
//! What stays in `<root>/.clew/` is the user's own data — bookmarks, notes,
//! the reading trail, tours, the reading target — and the project's config
//! (`lsp.toml`, `launch.json`). Those are meant to travel with the project,
//! are meaningful to a human, and are never treated as clew's own conclusions.
//! The per-reader part of it (trail, reading target, locks, temp files, the
//! generated-tour cache) is kept out of `git status` by the `.gitignore` the
//! state layer writes on first use (`statefile::CLEW_GITIGNORE`).
//!
//! # Privacy
//!
//! Everything here describes private code — LLM explanations of it, its
//! embeddings, its symbol index — so the data root and every directory clew
//! creates under it are user-only (0700, see [`ensure_private_dir`]),
//! whatever the umask. On a shared machine the default 0755 made all of it
//! readable by every other account.
//!
//! # Housekeeping of the data directory
//!
//! | directory       | holds                                   | cleaned up by |
//! |-----------------|-----------------------------------------|---------------|
//! | `cache/<key>/`  | one project's derived artifacts         | nobody, on purpose: a project unopened for months must not lose explanations the user paid for. Deleting a directory by hand is always safe — clew rebuilds (and re-bills) on the next open. |
//! | `exec/`         | approved repo-specified LSP commands    | `trust::sweep_exec`, after every stage: entries no approval references and nothing used for 30 days |
//! | `server-dist/`  | downloaded clew-server builds           | `server_dist`, after every fetch: other versions untouched for 7 days |
//! | `updates/`      | app update downloads                    | the updater's own stale-download sweep |

use std::path::{Path, PathBuf};

/// The derived-artifact directory for one project, created on demand (0700).
///
/// `host` is the SSH host for a remote project (`None` locally): the same
/// absolute path on two machines is two different projects, exactly as it is
/// for trust records. `None` when there is no data directory to write in — in
/// which case every cache simply runs in memory for the session.
pub fn dir(host: Option<&str>, root: &Path) -> Option<PathBuf> {
    let data = crate::lsp::store::data_root()?;
    let cache = data.join("cache");
    let dir = cache.join(key(host, root));
    ensure_private_dir(&data).ok()?;
    ensure_private_dir(&cache).ok()?;
    // Before the key was canonical, a project opened through a symlinked or
    // `..`-bearing path was filed under that spelling. Adopt such a directory
    // once, so the switch does not throw away (and re-bill) what it holds.
    if std::fs::symlink_metadata(&dir).is_err()
        && let Some(legacy) = legacy_key(host, root).map(|k| cache.join(k))
        && legacy != dir
        && legacy.is_dir()
    {
        let _ = std::fs::rename(&legacy, &dir);
    }
    ensure_private_dir(&dir).ok()?;
    Some(dir)
}

/// THE identity of a project on `host` (`None` = this machine), shared by
/// every record keyed by project: the trust store's roots and approvals, and
/// the derived-artifact directory here.
///
/// Locally the root is canonicalized, so a symlinked, relative or
/// `..`-bearing spelling of an already-known project is the same project —
/// two keys for one directory was how an approval and a cache could each be
/// found under a different spelling than the other. A root that cannot be
/// canonicalized (it no longer exists) keys by its absolute path, so a local
/// key is always absolute and never begins `ssh://` as a remote's does.
///
/// A remote root cannot be resolved from here, and the same path on two
/// hosts is two projects, so the host's label is part of the key and the
/// path is used verbatim: `ssh://<label>:<root>`, the form trust records have
/// always used. A relative root (`clew proj` with `CLEW_SSH` set) is written
/// `./proj`: as it was, `ssh://u@h:2222:/srv/p` was both `u@h` port 22's
/// `2222:/srv/p` and `u@h:2222`'s `/srv/p`. That the key names one label and
/// one root is proved in `verification/Clew/Trust.lean`, and
/// `clew_rules::approval` is proved to write that key.
///
/// A path is written as [`path_text`] writes it, so two paths never share a
/// key, whatever bytes they hold.
pub fn project_key(host: Option<&str>, root: &Path) -> String {
    match host {
        None => path_text(
            &root
                .canonicalize()
                .or_else(|_| std::path::absolute(root))
                .unwrap_or_else(|_| root.to_path_buf()),
        ),
        Some(h) => {
            let key = clew_rules::approval::approval_key(h.as_bytes(), path_text(root).as_bytes());
            // `ssh://`, `:` and `./` are ASCII between whole strings: UTF-8
            // in is UTF-8 out, so nothing is ever replaced here.
            String::from_utf8_lossy(&key).into_owned()
        }
    }
}

/// Create `dir` (and any missing parents) readable by the user only, or — if
/// it already exists, is a directory, and belongs to the user — take group
/// and other access away from it. A symlink at the name is followed: where
/// the data directory lives is the user's own choice.
///
/// An error means the directory is unusable (it is not a directory, or it
/// could not be created); failing to tighten someone else's directory is not
/// one — clew does not own it and leaves its mode alone.
#[cfg(unix)]
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {
            let own = meta.uid() == unsafe { libc::geteuid() };
            if own && meta.mode() & 0o077 != 0 {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Ok(_) => Err(std::io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir),
        Err(e) => Err(e),
    }
}

/// No POSIX modes to set: create the directory and rely on the platform's
/// per-user profile ACLs.
#[cfg(not(unix))]
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// A filesystem-safe, collision-resistant name for a project.
///
/// SHA-256 of [`project_key`]: paths contain separators and characters no
/// filesystem agrees on, and this value names a directory clew later writes
/// into, so it has to be exactly one plain component. Truncated to 32 hex
/// characters — 128 bits, far past any accidental collision, and short
/// enough that the directory stays readable in a file browser.
fn key(host: Option<&str>, root: &Path) -> String {
    hex128(&project_key(host, root))
}

/// A path as the text of a key: its UTF-8 as it is, and each byte of it that
/// is not UTF-8 (on Windows, each unpaired surrogate) as a NUL and its value
/// in hex. No path holds a NUL, so no two paths share a text — as they did
/// when every such byte became the one `\u{FFFD}` of `to_string_lossy`, and
/// `/p/\xff` and `/p/\xfe` were one project to approve. A UTF-8 path, which
/// is nearly every path, is its own text, so its key is what it always was.
pub fn path_text(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::fmt::Write;
        use std::os::unix::ffi::OsStrExt;
        let mut text = String::new();
        for chunk in path.as_os_str().as_bytes().utf8_chunks() {
            text.push_str(chunk.valid());
            for byte in chunk.invalid() {
                let _ = write!(text, "\0{byte:02x}");
            }
        }
        text
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        char::decode_utf16(path.as_os_str().encode_wide())
            .map(|unit| match unit {
                Ok(c) => c.to_string(),
                Err(e) => format!("\0{:04x}", e.unpaired_surrogate()),
            })
            .collect()
    }
    #[cfg(not(any(unix, windows)))]
    {
        path.to_string_lossy().into_owned()
    }
}

/// The key before it was canonical: the path exactly as the caller spelled
/// it. Only consulted to adopt an old directory (see [`dir`]) — and never
/// for a path that is not UTF-8, whose old key other paths shared.
fn legacy_key(host: Option<&str>, root: &Path) -> Option<String> {
    let root = root.to_str()?;
    let scoped = match host {
        None => root.to_string(),
        Some(h) => format!("ssh://{h}:{root}"),
    };
    Some(hex128(&scoped))
}

fn hex128(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(s.as_bytes());
    digest.iter().take(16).fold(String::new(), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_one_plain_component_and_host_scoped() {
        let root = Path::new("/home/u/projects/clew");
        let local = key(None, root);
        let remote = key(Some("u@host"), root);
        let other_host = key(Some("u@host:2222"), root);

        assert_eq!(local.len(), 32);
        assert_ne!(
            local, remote,
            "the same path on another machine is another project"
        );
        assert_ne!(remote, other_host, "and so is another port on the same one");
        // Must be usable as a single directory name: no separators, no
        // traversal, nothing a filesystem would reinterpret.
        for k in [&local, &remote, &other_host] {
            assert!(k.chars().all(|c| c.is_ascii_hexdigit()), "{k}");
            assert_eq!(
                Path::new(k).components().count(),
                1,
                "{k} must be exactly one component"
            );
        }
    }

    #[test]
    fn different_projects_get_different_directories() {
        assert_ne!(
            key(None, Path::new("/a/project")),
            key(None, Path::new("/b/project"))
        );
    }

    /// The drift this fixes: trust keyed a project by its canonical path while
    /// the cache keyed it by the spelling it was opened under, so the same
    /// directory was two projects to one and one to the other.
    #[test]
    #[cfg(unix)]
    fn every_spelling_of_a_local_project_is_one_key() {
        let base = crate::testutil::TempDir::new("derived-spellings");
        let real = base.join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::os::unix::fs::symlink(&real, base.join("link")).unwrap();

        let canonical = project_key(None, &real);
        assert_eq!(project_key(None, &base.join("link")), canonical);
        assert_eq!(project_key(None, &real.join("sub").join("..")), canonical);
        assert_eq!(key(None, &base.join("link")), key(None, &real));
        // Remote paths are never resolved here, and never lose their host.
        assert_eq!(
            project_key(Some("u@h"), Path::new("/srv/p")),
            "ssh://u@h:/srv/p"
        );
        // A relative remote root can never read as a port and a root.
        assert_eq!(
            project_key(Some("u@h"), Path::new("2222:/srv/p")),
            "ssh://u@h:./2222:/srv/p"
        );
        assert_ne!(
            project_key(Some("u@h"), Path::new("2222:/srv/p")),
            project_key(Some("u@h:2222"), Path::new("/srv/p"))
        );
        // A local root that does not exist is keyed by its absolute path.
        let gone = project_key(None, Path::new("no-such-project-here"));
        assert!(Path::new(&gone).is_absolute(), "{gone}");
    }

    /// Two paths that are not UTF-8 are two projects: their bytes are kept,
    /// not each made the same replacement character.
    #[test]
    #[cfg(unix)]
    fn paths_that_are_not_utf8_keep_their_bytes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let a = Path::new(OsStr::from_bytes(b"/srv/p\xff"));
        let b = Path::new(OsStr::from_bytes(b"/srv/p\xfe"));
        assert_eq!(path_text(a), "/srv/p\0ff");
        assert_ne!(project_key(None, a), project_key(None, b));
        assert_ne!(project_key(Some("u@h"), a), project_key(Some("u@h"), b));
        assert_ne!(key(None, a), key(None, b));
        // Nor is either the path that holds the replacement character.
        assert_ne!(
            project_key(None, a),
            project_key(None, Path::new("/srv/p\u{FFFD}"))
        );
        // And a UTF-8 path is its own text, so its key never changed.
        assert_eq!(path_text(Path::new("/srv/é p")), "/srv/é p");
        assert_eq!(legacy_key(None, a), None);
    }

    /// The data directory and the per-project cache are user-only, and a
    /// directory an older clew left world-readable is tightened; an old
    /// cache filed under a non-canonical spelling is adopted, not orphaned.
    #[test]
    #[cfg(unix)]
    fn cache_dirs_are_private_and_legacy_spellings_are_adopted() {
        use std::os::unix::fs::PermissionsExt;
        // `CLEW_DATA_DIR` at a fresh directory, restored when this drops.
        let data_dir = crate::testutil::DataDir::new("derived-private");
        let data = data_dir.to_path_buf();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
        let base = crate::testutil::TempDir::new("derived-private-projects");
        let project = base.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::os::unix::fs::symlink(&project, base.join("alias")).unwrap();

        // An older clew filed the project under the alias spelling.
        let legacy = data
            .join("cache")
            .join(legacy_key(None, &base.join("alias")).unwrap());
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("explain.json"), "{}").unwrap();

        let dir = dir(None, &base.join("alias")).expect("a cache dir");

        assert_eq!(dir, data.join("cache").join(key(None, &project)));
        assert!(
            dir.join("explain.json").is_file(),
            "the legacy directory was adopted"
        );
        assert!(!legacy.exists());
        for d in [&data, &data.join("cache"), &dir] {
            let mode = std::fs::metadata(d).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} is {mode:o}", d.display());
        }
    }
}
