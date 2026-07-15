//! Project scanning: builds the directory tree and the flat file list,
//! honoring `.gitignore` (via the `ignore` crate).

use std::collections::BTreeMap;
use std::path::PathBuf;

use ignore::WalkBuilder;

/// Hard cap on scanned entries to keep giant repos responsive.
pub const MAX_ENTRIES: usize = 100_000;

/// A directory node: sub-directories first, then files (both sorted).
#[derive(Debug, Clone, Default)]
pub struct DirNode {
    pub dirs: Vec<(String, DirNode)>,
    pub files: Vec<String>,
}

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
        .filter_entry(|entry| entry.file_name() != ".git" && entry.file_name() != ".clew")
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
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
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
}
