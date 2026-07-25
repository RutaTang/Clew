//! Per-project bookmarks.
//!
//! All persisted state lives with the project in `<root>/.clew/` — nothing
//! is ever written outside the project directory. The `.clew/` directory is
//! created when the user consents at project-open time and doubles as the
//! consent record, so it is never removed here; an emptied store only
//! removes its own file. If saving fails (e.g. `.clew` was deleted while
//! running), the caller surfaces the error instead of silently dropping data.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub rel: String,
    pub line: usize, // 1-based
    pub preview: String,
    /// Optional freeform (plain-text) note the reader attached.
    #[serde(default)]
    pub note: Option<String>,
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

    // No bookmarks left: remove the store file. The .clew directory stays —
    // it records the user's consent to keep clew data in this project.
    if bookmarks.is_empty() {
        let _ = std::fs::remove_file(&path);
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
            note: None,
        });
        list.sort_by(|a, b| a.rel.cmp(&b.rel).then(a.line.cmp(&b.line)));
        true
    }
}

/// Set (or clear, when `None`/empty) the note on the bookmark at `rel:line`.
pub fn set_note(list: &mut [Bookmark], rel: &str, line: usize, note: Option<String>) {
    if let Some(b) = list.iter_mut().find(|b| b.rel == rel && b.line == line) {
        b.note = note.filter(|s| !s.trim().is_empty());
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
            note: None,
        }];
        save(&root, &list).unwrap();
        assert!(root.join(".clew/bookmarks.json").exists());
        assert_eq!(load(&root), list);

        // Removing the last bookmark removes the store file but keeps the
        // .clew directory (it records open-time consent).
        save(&root, &[]).unwrap();
        assert!(!root.join(".clew/bookmarks.json").exists());
        assert!(root.join(".clew").is_dir());
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
            note: None,
        }];
        assert!(save(&root, &list).is_err());
    }
}
