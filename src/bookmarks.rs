//! Per-project bookmarks.
//!
//! All persisted state lives with the project in `<root>/.clew/` — nothing
//! is ever written outside the project directory. The directory is created
//! lazily on the first save and removed again when the last bookmark goes.
//! On read-only project directories saving fails; the caller surfaces that
//! to the user instead of silently dropping data.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub rel: String,
    pub line: usize, // 1-based
    pub preview: String,
}

fn store_path(root: &Path) -> PathBuf {
    root.join(".clew").join("bookmarks.json")
}

pub fn load(root: &Path) -> Vec<Bookmark> {
    std::fs::read_to_string(store_path(root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(root: &Path, bookmarks: &[Bookmark]) -> std::io::Result<()> {
    let path = store_path(root);

    // No bookmarks left: remove the store instead of leaving junk behind.
    if bookmarks.is_empty() {
        let _ = std::fs::remove_file(&path);
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir(dir); // only succeeds when empty
        }
        return Ok(());
    }

    let json = serde_json::to_string_pretty(bookmarks)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, json)
}

/// Toggle a bookmark; returns true when one was added.
pub fn toggle(list: &mut Vec<Bookmark>, rel: &str, line: usize, preview: String) -> bool {
    if let Some(pos) = list.iter().position(|b| b.rel == rel && b.line == line) {
        list.remove(pos);
        false
    } else {
        list.push(Bookmark {
            rel: rel.to_string(),
            line,
            preview,
        });
        list.sort_by(|a, b| a.rel.cmp(&b.rel).then(a.line.cmp(&b.line)));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_adds_sorts_and_removes() {
        let mut list = Vec::new();
        assert!(toggle(&mut list, "b.rs", 10, "x".into()));
        assert!(toggle(&mut list, "a.rs", 5, "y".into()));
        assert_eq!(list[0].rel, "a.rs");
        assert!(!toggle(&mut list, "b.rs", 10, String::new()));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn saves_into_project_clew_dir_and_cleans_up() {
        let root = std::env::temp_dir().join("clew-bm-project-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let list = vec![Bookmark {
            rel: "src/main.rs".into(),
            line: 42,
            preview: "fn main() {".into(),
        }];
        save(&root, &list).unwrap();
        assert!(root.join(".clew/bookmarks.json").exists());
        assert_eq!(load(&root), list);

        // Removing the last bookmark removes the store and the .clew dir.
        save(&root, &[]).unwrap();
        assert!(!root.join(".clew").exists());
        assert!(load(&root).is_empty());
    }

    #[test]
    fn save_fails_on_unwritable_root_without_touching_elsewhere() {
        let root = std::env::temp_dir().join("clew-bm-readonly-test/nonexistent-parent");
        // Parent chain cannot be created inside a file path: make a file at
        // the would-be root parent to force create_dir_all to fail.
        let base = std::env::temp_dir().join("clew-bm-readonly-test");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_file(&base);
        std::fs::write(&base, "not a dir").unwrap();

        let list = vec![Bookmark {
            rel: "a.rs".into(),
            line: 1,
            preview: String::new(),
        }];
        assert!(save(&root, &list).is_err());
    }
}
