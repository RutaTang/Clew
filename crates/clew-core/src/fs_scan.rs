//! Project scanning: builds the directory tree and the flat file list,
//! honoring `.gitignore` (via the `ignore` crate).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

/// Hard cap on scanned entries to keep giant repos responsive.
pub const MAX_ENTRIES: usize = 100_000;

/// How deeply the emitted [`DirNode`] tree may nest before the rest of a path
/// is folded into the row's own name.
///
/// Nesting is repository-controlled and was unbounded: `MAX_ENTRIES` bounds how
/// many entries are kept, not how deep they sit, since 45 nested directories
/// are only 45 entries. Every frame crosses the transport as NDJSON — including
/// a LOCAL project, whose server is a child process over stdio — and
/// `serde_json`'s deserializer refuses more than 128 nested containers. One
/// directory level costs three of them (`dirs` array, tuple array, child
/// object), so past ~41 levels the client cannot parse the `Tree` frame AT ALL,
/// however small it is: it drops the link as unreadable and respawns into the
/// identical frame, so the project can never be opened. No node is deeper than
/// this, which is well inside that ceiling and inside the stack that `convert`
/// (and `DirNode`'s recursive drop) can afford.
pub const MAX_TREE_DEPTH: usize = 32;

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
                break;
            }
            if i == MAX_TREE_DEPTH {
                // `node` is the deepest node the wire format can carry. A file
                // below it becomes a row OF `node`, named by its whole
                // remaining path: the client builds each row's rel by joining
                // names with `/`, so the row still opens the same file, and the
                // slashes in the name are what tell the reader the nesting
                // below this point was folded away.
                //
                // A directory gets no row of its own. Giving each folded
                // directory one would repeat every ancestor's name inside every
                // descendant's, so a single deep chain would cost bytes
                // quadratic in its length — the frame is what we are trying to
                // keep sendable. The cost is that a directory nested past the
                // cap and holding no file anywhere below it has no row at all;
                // it would have rendered as an empty folder. Files are never
                // lost: `files` below carries every rel in full, so search,
                // docs and the agent are unaffected.
                if !is_dir {
                    node.files.push(comps[i..].join("/"));
                }
                break;
            }
            node = node.dirs.entry(name.clone()).or_default();
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
/// inside `root` or larger than `max_bytes`.
///
/// Opened ONCE, through [`crate::statefile::open_plain`], and read through the
/// cap. The obvious spelling — check the path, stat the path, then read the
/// path — resolves the name three times and enforces the size on a stat the
/// read never sees. That let a file growing between the two past the cap, and
/// a path swapped for a FIFO in the same window blocked `read_to_string`
/// forever, wedging the indexer thread (and the publication lock it holds)
/// for the life of the process.
pub fn read_confined_capped(root: &Path, path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::Read;
    // Containment is still decided from the path, so a swapped PARENT
    // directory remains the accepted residual documented for `.clew`. The
    // leaf, which is what actually gets read, is now safe on its own:
    // `O_NOFOLLOW` refuses a symlink and `O_NONBLOCK` refuses to block.
    if !is_inside(root, path) {
        return None;
    }
    let f = crate::statefile::open_plain(path)?;
    if f.metadata().ok()?.len() > max_bytes {
        return None; // cheap early reject, before reading a byte
    }
    let mut s = String::new();
    // `max_bytes + 1`: reading one byte past the cap is what distinguishes
    // "exactly at the limit" from "grew past it while we were reading".
    f.take(max_bytes + 1).read_to_string(&mut s).ok()?;
    (s.len() as u64 <= max_bytes).then_some(s)
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
        const CAP: u64 = 1024 * 1024;
        assert!(read_confined_capped(&dir, &dir.join("outside.txt"), CAP).is_none());
        assert_eq!(
            read_confined_capped(&dir, &dir.join("src/main.rs"), CAP).as_deref(),
            Some("fn main() {}\n")
        );
    }

    /// A deep directory must still produce a tree the CLIENT can parse. The
    /// frame here is a few KB — the failure this guards is `serde_json`'s
    /// 128-container recursion limit, not the frame cap — and it is fatal:
    /// an unparseable frame drops the connection, and the respawn rebuilds the
    /// same frame forever.
    #[test]
    fn deep_nesting_stays_within_the_wire_recursion_limit() {
        const DEPTH: usize = 60; // comfortably past MAX_TREE_DEPTH
        let dir = std::env::temp_dir().join("clew-scan-depth-test");
        let _ = std::fs::remove_dir_all(&dir);
        let mut deep = dir.clone();
        let mut rel = String::new();
        for i in 0..DEPTH {
            deep = deep.join(format!("d{i}"));
            if i > 0 {
                rel.push('/');
            }
            rel.push_str(&format!("d{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("leaf.rs"), "fn leaf() {}\n").unwrap();

        let result = scan(dir.clone());

        // The flat file list is untouched by the fold: every file keeps its
        // full rel, so search, docs and the agent still see it.
        let rels: Vec<&str> = result.files.iter().map(|f| f.rel.as_str()).collect();
        let leaf_rel = format!("{rel}/leaf.rs");
        assert!(
            rels.contains(&leaf_rel.as_str()),
            "deep file lost: {rels:?}"
        );

        // The assertion that was missing: the real wire envelope round-trips.
        let msg = clew_protocol::ServerMessage::Notification {
            sub: None,
            event: clew_protocol::Event::Tree {
                root: result.root.to_string_lossy().into_owned(),
                tree: result.tree.clone(),
                files: result.files.iter().map(|f| f.rel.clone()).collect(),
                truncated: result.truncated,
            },
        };
        let line = serde_json::to_string(&msg).unwrap();
        let back = serde_json::from_str::<clew_protocol::ServerMessage>(&line);
        assert!(
            back.is_ok(),
            "the client cannot parse its own Tree frame: {:?}",
            back.err()
        );

        // The tree is bounded, and the fold is visible in the tree itself: the
        // deep file appears as one row whose name is its whole remaining path.
        fn walk(node: &DirNode, depth: usize, deepest: &mut usize, folded: &mut Vec<String>) {
            *deepest = (*deepest).max(depth);
            folded.extend(node.files.iter().filter(|f| f.contains('/')).cloned());
            for (_, child) in &node.dirs {
                walk(child, depth + 1, deepest, folded);
            }
        }
        let (mut deepest, mut folded) = (0usize, Vec::new());
        walk(&result.tree, 0, &mut deepest, &mut folded);
        assert!(
            deepest <= MAX_TREE_DEPTH,
            "tree nests {deepest} levels, past the cap"
        );
        assert_eq!(folded.len(), 1, "expected one folded row, got {folded:?}");
        // Joining row names the way the sidebar does still yields the real rel,
        // so the folded row opens exactly the file it names.
        assert_eq!(
            format!(
                "{}/{}",
                (0..MAX_TREE_DEPTH)
                    .map(|i| format!("d{i}"))
                    .collect::<Vec<_>>()
                    .join("/"),
                folded[0]
            ),
            leaf_rel
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cap belongs on the READ, not on a separate stat: a file can grow
    /// between the two, and the type check has to hold for the handle that is
    /// actually read — a FIFO swapped in would otherwise block forever.
    #[test]
    fn read_confined_capped_bounds_the_read_and_refuses_a_fifo() {
        let dir = std::env::temp_dir().join("clew-read-capped-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.rs");
        std::fs::write(&file, "x".repeat(4096)).unwrap();

        assert!(
            read_confined_capped(&dir, &file, 1024).is_none(),
            "over cap"
        );
        assert_eq!(
            read_confined_capped(&dir, &file, 8192).map(|s| s.len()),
            Some(4096)
        );
        // Exactly at the limit is allowed; one byte more is not.
        assert!(read_confined_capped(&dir, &file, 4096).is_some());
        assert!(read_confined_capped(&dir, &file, 4095).is_none());

        #[cfg(unix)]
        {
            let pipe = dir.join("pipe.rs");
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&pipe)
                    .status()
                    .is_ok_and(|s| s.success())
            );
            // On a thread, so a regression is a failed assert rather than a
            // test run that never finishes.
            let (tx, rx) = std::sync::mpsc::channel();
            let (d, p) = (dir.clone(), pipe.clone());
            std::thread::spawn(move || {
                let _ = tx.send(read_confined_capped(&d, &p, 8192).is_none());
            });
            assert!(
                rx.recv_timeout(std::time::Duration::from_secs(5))
                    .expect("the read blocked on the FIFO"),
                "a FIFO must not be read as a project file"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
