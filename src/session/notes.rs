//! Per-project reading notes and progress.
//!
//! A reading note is anchored to a SYMBOL (file + symbol name), never a raw
//! line. That is what makes it survive edits and re-scans: the current line is
//! resolved from the live symbol index at display time, so there is nothing to
//! migrate on refresh, and a note whose symbol has vanished (renamed/deleted)
//! surfaces as detached rather than silently pointing at the wrong code.
//!
//! Persisted with the project in `<root>/.clew/notes.json` (atomic write);
//! nothing is written outside the project. Like the other stores, an emptied
//! list removes its own file but keeps `.clew/` (the open-time consent record).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One reading annotation on a symbol: an "understood" flag and/or a note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    /// Project-relative path of the file the symbol is in.
    pub rel: String,
    /// The symbol's name — the stable anchor (the line is resolved live).
    pub symbol: String,
    /// Whether the reader has marked this symbol as understood.
    #[serde(default)]
    pub understood: bool,
    /// Freeform plain-text note (empty = none).
    #[serde(default)]
    pub text: String,
}

impl Note {
    /// A note that carries no information can be dropped from the store.
    fn is_empty(&self) -> bool {
        !self.understood && self.text.trim().is_empty()
    }
}

fn store_path(root: &Path) -> PathBuf {
    root.join(".clew").join("notes.json")
}

pub fn load(root: &Path) -> Vec<Note> {
    std::fs::read_to_string(store_path(root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the notes (atomic temp+rename). An empty list removes the file.
pub fn save(root: &Path, notes: &[Note]) -> std::io::Result<()> {
    let path = store_path(root);
    if notes.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json =
        serde_json::to_string_pretty(notes).map_err(|e| std::io::Error::other(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// The note for `(rel, symbol)`, if any.
pub fn find<'a>(list: &'a [Note], rel: &str, symbol: &str) -> Option<&'a Note> {
    list.iter().find(|n| n.rel == rel && n.symbol == symbol)
}

fn sort(list: &mut [Note]) {
    list.sort_by(|a, b| a.rel.cmp(&b.rel).then_with(|| a.symbol.cmp(&b.symbol)));
}

/// Toggle the "understood" flag for `(rel, symbol)`, creating the note when
/// absent and dropping it if it becomes empty. Returns the resulting state.
pub fn toggle_understood(list: &mut Vec<Note>, rel: &str, symbol: &str) -> bool {
    if let Some(pos) = list.iter().position(|n| n.rel == rel && n.symbol == symbol) {
        list[pos].understood = !list[pos].understood;
        let state = list[pos].understood;
        if list[pos].is_empty() {
            list.remove(pos);
        }
        state
    } else {
        list.push(Note {
            rel: rel.into(),
            symbol: symbol.into(),
            understood: true,
            text: String::new(),
        });
        sort(list);
        true
    }
}

/// Set (or clear, when blank) the note text for `(rel, symbol)`, creating or
/// dropping the note as needed.
pub fn set_text(list: &mut Vec<Note>, rel: &str, symbol: &str, text: &str) {
    let text = text.trim();
    match list.iter().position(|n| n.rel == rel && n.symbol == symbol) {
        Some(pos) => {
            list[pos].text = text.to_string();
            if list[pos].is_empty() {
                list.remove(pos);
            }
        }
        None if !text.is_empty() => {
            list.push(Note {
                rel: rel.into(),
                symbol: symbol.into(),
                understood: false,
                text: text.to_string(),
            });
            sort(list);
        }
        None => {}
    }
}

/// Remove the note for `(rel, symbol)`.
pub fn remove(list: &mut Vec<Note>, rel: &str, symbol: &str) {
    list.retain(|n| !(n.rel == rel && n.symbol == symbol));
}

/// `(understood, total)` symbol counts for one file, given its live symbol
/// names — the coverage always reflects the current index, so it self-corrects
/// after a re-scan.
pub fn coverage(list: &[Note], rel: &str, symbols: &[String]) -> (usize, usize) {
    let understood = symbols
        .iter()
        .filter(|name| find(list, rel, name).is_some_and(|n| n.understood))
        .count();
    (understood, symbols.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_creates_flips_and_drops() {
        let mut list = Vec::new();
        assert!(toggle_understood(&mut list, "a.rs", "foo"));
        assert_eq!(list.len(), 1);
        // Flipping back to false with no text drops the empty note.
        assert!(!toggle_understood(&mut list, "a.rs", "foo"));
        assert!(list.is_empty());
    }

    #[test]
    fn text_kept_when_unmarking_understood() {
        let mut list = Vec::new();
        toggle_understood(&mut list, "a.rs", "foo");
        set_text(&mut list, "a.rs", "foo", "  a note  ");
        assert_eq!(find(&list, "a.rs", "foo").unwrap().text, "a note");
        // Un-understanding keeps the note because it still has text.
        assert!(!toggle_understood(&mut list, "a.rs", "foo"));
        assert_eq!(list.len(), 1);
        // Clearing the text now drops it (no flag, no text).
        set_text(&mut list, "a.rs", "foo", "");
        assert!(list.is_empty());
    }

    #[test]
    fn coverage_counts_only_live_symbols() {
        let mut list = Vec::new();
        toggle_understood(&mut list, "a.rs", "foo");
        toggle_understood(&mut list, "a.rs", "gone"); // symbol later deleted
        // The live index only has `foo` and `bar`; `gone` is not counted.
        let syms = vec!["foo".to_string(), "bar".to_string()];
        assert_eq!(coverage(&list, "a.rs", &syms), (1, 2));
    }

    #[test]
    fn roundtrips_through_disk() {
        let root = std::env::temp_dir().join("clew-notes-roundtrip-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut list = Vec::new();
        toggle_understood(&mut list, "src/main.rs", "main");
        set_text(&mut list, "src/main.rs", "main", "entry point");
        save(&root, &list).unwrap();
        assert!(root.join(".clew/notes.json").exists());
        assert_eq!(load(&root), list);
        // Emptying removes the file but keeps .clew/.
        save(&root, &[]).unwrap();
        assert!(!root.join(".clew/notes.json").exists());
        assert!(root.join(".clew").is_dir());
    }
}
