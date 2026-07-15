//! Per-project bookmarks.
//!
//! Bookmarks live with the project in `<root>/.clew/bookmarks.json` so they
//! survive moving the project and can be shared. The directory is created
//! lazily on the first save. When the project directory is not writable
//! (read-only checkouts), we fall back to a store in the user data dir.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub rel: String,
    pub line: usize, // 1-based
    pub preview: String,
}

fn project_store(root: &Path) -> PathBuf {
    root.join(".clew").join("bookmarks.json")
}

/// Stable hash so each project root maps to one fallback store file.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn fallback_store(root: &Path) -> Option<PathBuf> {
    let base = match std::env::var_os("CLEW_DATA_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => dirs::data_dir()?.join("clew"),
    };
    let key = fnv1a(root.to_string_lossy().as_bytes());
    Some(base.join("bookmarks").join(format!("{key:016x}.json")))
}

fn read_store(path: &Path) -> Option<Vec<Bookmark>> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn load(root: &Path) -> Vec<Bookmark> {
    if let Some(list) = read_store(&project_store(root)) {
        return list;
    }
    fallback_store(root)
        .and_then(|p| read_store(&p))
        .unwrap_or_default()
}

pub fn save(root: &Path, bookmarks: &[Bookmark]) {
    let project_path = project_store(root);

    // No bookmarks left: remove the store instead of leaving junk behind.
    if bookmarks.is_empty() {
        let _ = std::fs::remove_file(&project_path);
        if let Some(dir) = project_path.parent() {
            let _ = std::fs::remove_dir(dir); // only succeeds when empty
        }
        if let Some(fallback) = fallback_store(root) {
            let _ = std::fs::remove_file(fallback);
        }
        return;
    }

    let Ok(json) = serde_json::to_string_pretty(bookmarks) else {
        return;
    };
    if write_store(&project_path, &json).is_ok() {
        return;
    }
    // Project directory not writable: fall back to the user data dir.
    if let Some(fallback) = fallback_store(root) {
        let _ = write_store(&fallback, &json);
    }
}

fn write_store(path: &Path, json: &str) -> std::io::Result<()> {
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
        save(&root, &list);
        assert!(root.join(".clew/bookmarks.json").exists());
        assert_eq!(load(&root), list);

        // Removing the last bookmark removes the store and the .clew dir.
        save(&root, &[]);
        assert!(!root.join(".clew").exists());
        assert!(load(&root).is_empty());
    }
}
