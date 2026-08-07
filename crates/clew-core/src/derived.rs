//! Where clew keeps the artifacts it DERIVES from a project — the symbol
//! index, explanations, embeddings, the overview, statistics, rendered SVGs.
//!
//! Never inside the project. These used to live in `<root>/.clew/cache/`,
//! which the repository controls: every entry is keyed by a content hash the
//! repository can compute for its own files, so a committed cache is accepted
//! as clew's own work. That turns a checkout into a way to forge the symbol
//! index (and with it navigation, the graphs, the overview, and the source
//! clew hands to the model) with no code execution at all. The module that
//! read it justified itself with "`.clew/` is git-ignored, so the cache is
//! strictly local" — nothing in clew writes a `.gitignore`, and the scanner
//! prunes `.clew` unconditionally, so a committed cache is also invisible.
//!
//! The fix is not to authenticate the bytes but to stop the repository from
//! having a say: derived artifacts move to clew's own data directory, keyed
//! by the project. That also makes a read-only checkout cache normally.
//!
//! What stays in `<root>/.clew/` is the user's own data — bookmarks, notes,
//! the reading trail, tours, the reading target — and the project's config
//! (`lsp.toml`, `launch.json`). Those are meant to travel with the project,
//! are meaningful to a human, and are never treated as clew's own conclusions.

use std::path::{Path, PathBuf};

/// The derived-artifact directory for one project, created on demand.
///
/// `host` is the SSH host for a remote project (`None` locally): the same
/// absolute path on two machines is two different projects, exactly as it is
/// for trust records. `None` when there is no data directory to write in — in
/// which case every cache simply runs in memory for the session.
pub fn dir(host: Option<&str>, root: &Path) -> Option<PathBuf> {
    let dir = crate::lsp::store::data_root()?
        .join("cache")
        .join(key(host, root));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// A filesystem-safe, collision-resistant name for a project.
///
/// SHA-256 of the host-scoped path: paths contain separators and characters
/// no filesystem agrees on, and this value names a directory clew later
/// writes into, so it has to be exactly one plain component. Truncated to 32
/// hex characters — 128 bits, far past any accidental collision, and short
/// enough that the directory stays readable in a file browser.
fn key(host: Option<&str>, root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let scoped = match host {
        None => root.to_string_lossy().into_owned(),
        Some(h) => format!("ssh://{h}:{}", root.to_string_lossy()),
    };
    let digest = Sha256::digest(scoped.as_bytes());
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
}
