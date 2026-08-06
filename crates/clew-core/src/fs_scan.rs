//! Project scanning: builds the directory tree and the flat file list,
//! honoring `.gitignore` (via the `ignore` crate).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

/// Hard cap on scanned entries to keep giant repos responsive.
pub const MAX_ENTRIES: usize = 100_000;

/// Directories a code reader should never scan: clew/git internals, and the
/// build-output / vendored-dependency dirs of the supported languages. Skipped
/// unconditionally (even without a `.gitignore`, which many projects lack) so
/// the file tree and symbol index stay about the reader's own source — e.g. a
/// TS project's `node_modules` `.d.ts` files would otherwise flood the index.
fn is_ignored_dir(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            ".git"
                | ".clew"
                | "node_modules"
                | "target"
                | ".dart_tool"
                | ".venv"
                | "venv"
                | "__pycache__"
        )
    )
}

// The directory tree is a protocol wire type: the scanner builds it here, the
// server transmits it, the client renders it verbatim.
pub use clew_protocol::DirNode;

/// A file known to the project, with absolute path and root-relative display path.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub abs: PathBuf,
    pub rel: String,
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub root: PathBuf,
    pub tree: DirNode,
    pub files: Vec<FileEntry>,
    pub truncated: bool,
}

#[derive(Default)]
struct TmpDir {
    dirs: BTreeMap<String, TmpDir>,
    files: Vec<String>,
}

/// Walk `root` and build the tree + flat file list. Blocking; run off the UI thread.
pub fn scan(root: PathBuf) -> ScanResult {
    let mut tmp = TmpDir::default();
    let mut files = Vec::new();
    let mut truncated = false;
    let mut seen = 0usize;

    let walker = WalkBuilder::new(&root)
        .hidden(false) // show dotfiles; tool-internal dirs are filtered below
        .follow_links(false)
        .filter_entry(|entry| !is_ignored_dir(entry.file_name()))
        .build();

    for entry in walker.flatten() {
        if seen >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let path = entry.path();
        if path == root {
            continue;
        }
        let Ok(rel) = path.strip_prefix(&root) else {
            continue;
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir();
        // Only real directories and regular files are part of a project. A
        // symlink would read whatever it points at — including outside the
        // project — and a FIFO or device node would block or stream forever.
        if !is_dir && !file_type.is_file() {
            continue;
        }
        seen += 1;

        // Insert into the temporary tree.
        let comps: Vec<String> = rel
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect();
        let mut node = &mut tmp;
        for (i, name) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            if last && !is_dir {
                node.files.push(name.clone());
            } else {
                node = node.dirs.entry(name.clone()).or_default();
            }
        }

        if !is_dir {
            files.push(FileEntry {
                abs: path.to_path_buf(),
                rel: comps.join("/"),
            });
        }
    }

    files.sort_by(|a, b| a.rel.to_lowercase().cmp(&b.rel.to_lowercase()));

    ScanResult {
        root,
        tree: convert(tmp),
        files,
        truncated,
    }
}

/// Whether `path` is a regular file that really lives inside `root`.
///
/// The scan already excludes symlinks, but a path can be swapped for one
/// afterwards (or arrive from elsewhere), so every read re-checks: canonicalize
/// both sides and require containment. Cheap next to the read that follows.
pub fn is_inside(root: &Path, path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false; // symlink, FIFO, device, directory
    }
    let (Ok(real), Ok(real_root)) = (path.canonicalize(), root.canonicalize()) else {
        return false;
    };
    real.starts_with(&real_root)
}

/// Read a project file as text, refusing anything that isn't a regular file
/// inside `root`.
pub fn read_confined(root: &Path, path: &Path) -> Option<String> {
    is_inside(root, path)
        .then(|| std::fs::read_to_string(path).ok())
        .flatten()
}

fn convert(tmp: TmpDir) -> DirNode {
    let mut dirs: Vec<(String, DirNode)> = tmp
        .dirs
        .into_iter()
        .map(|(name, child)| (name, convert(child)))
        .collect();
    dirs.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    let mut node_files = tmp.files;
    node_files.sort_by_key(|a| a.to_lowercase());
    DirNode {
        dirs,
        files: node_files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_respects_gitignore_and_builds_tree() {
        let dir = std::env::temp_dir().join("clew-scan-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap(); // make gitignore apply
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("target/debug/junk.o"), "x").unwrap();
        std::fs::write(dir.join("README.md"), "# hi\n").unwrap();

        let result = scan(dir.clone());
        let rels: Vec<&str> = result.files.iter().map(|f| f.rel.as_str()).collect();
        assert!(rels.contains(&"src/main.rs"), "files: {rels:?}");
        assert!(rels.contains(&"README.md"), "files: {rels:?}");
        assert!(
            !rels.iter().any(|r| r.starts_with("target")),
            "gitignored files leaked: {rels:?}"
        );
        assert!(
            !rels.iter().any(|r| r.starts_with(".git/")),
            ".git leaked: {rels:?}"
        );

        // Tree: dirs sorted first, then files.
        let dir_names: Vec<&str> = result.tree.dirs.iter().map(|(n, _)| n.as_str()).collect();
        assert!(dir_names.contains(&"src"));
        assert!(!dir_names.contains(&"target"));
        assert!(result.tree.files.contains(&"README.md".to_string()));
    }

    #[test]
    fn skips_vendor_dirs_without_gitignore() {
        // No .git / .gitignore here — vendor/build dirs must still be pruned.
        let dir = std::env::temp_dir().join("clew-scan-vendor-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/lodash")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.join(".dart_tool")).unwrap();
        std::fs::write(dir.join("src/index.ts"), "export const x = 1;\n").unwrap();
        std::fs::write(
            dir.join("node_modules/lodash/index.d.ts"),
            "export declare const y: number;\n",
        )
        .unwrap();
        std::fs::write(dir.join("target/debug/build.json"), "{}").unwrap();
        std::fs::write(dir.join(".dart_tool/pkg.json"), "{}").unwrap();

        let result = scan(dir);
        let rels: Vec<&str> = result.files.iter().map(|f| f.rel.as_str()).collect();
        assert!(
            rels.contains(&"src/index.ts"),
            "own source missing: {rels:?}"
        );
        assert!(
            !rels.iter().any(|r| r.contains("node_modules")),
            "node_modules leaked: {rels:?}"
        );
        assert!(
            !rels.iter().any(|r| r.starts_with("target")),
            "target leaked: {rels:?}"
        );
        assert!(
            !rels.iter().any(|r| r.contains(".dart_tool")),
            ".dart_tool leaked: {rels:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_project_files() {
        // A repository can ship symlinks pointing anywhere on the host
        // (`outside.txt -> /etc/hosts`). They must not become project files:
        // search, docs, and the agent's `read` would follow them out.
        let dir = std::env::temp_dir().join("clew-scan-symlink-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();

        let secret_dir = std::env::temp_dir().join("clew-scan-symlink-secret");
        let _ = std::fs::remove_dir_all(&secret_dir);
        std::fs::create_dir_all(&secret_dir).unwrap();
        std::fs::write(secret_dir.join("secret.txt"), "s3cret\n").unwrap();

        std::os::unix::fs::symlink(secret_dir.join("secret.txt"), dir.join("outside.txt")).unwrap();
        std::os::unix::fs::symlink(&secret_dir, dir.join("outside-dir")).unwrap();
        // A link pointing *inside* the project is excluded too: reads resolve
        // paths, and only real files should ever be read.
        std::os::unix::fs::symlink(dir.join("src/main.rs"), dir.join("alias.rs")).unwrap();

        let result = scan(dir.clone());
        let rels: Vec<&str> = result.files.iter().map(|f| f.rel.as_str()).collect();
        assert!(rels.contains(&"src/main.rs"), "files: {rels:?}");
        assert!(
            !rels.iter().any(|r| r.contains("outside")),
            "symlink leaked into the scan: {rels:?}"
        );
        assert!(
            !rels.contains(&"alias.rs"),
            "in-project symlink leaked: {rels:?}"
        );

        // The read-time re-check agrees: a symlink is refused even when named
        // directly (the scan can be stale, or the path may come from elsewhere).
        assert!(is_inside(&dir, &dir.join("src/main.rs")));
        assert!(!is_inside(&dir, &dir.join("outside.txt")));
        assert!(!is_inside(&dir, &secret_dir.join("secret.txt")));
        assert!(read_confined(&dir, &dir.join("outside.txt")).is_none());
        assert_eq!(
            read_confined(&dir, &dir.join("src/main.rs")).as_deref(),
            Some("fn main() {}\n")
        );
    }
}
