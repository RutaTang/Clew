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

/// Decode a store file's text. Shared by the local disk path and the remote
/// protocol path (`StateContent`); rel paths are validated either way — the
/// text is repo-shipped (or remote-supplied) and a crafted entry must not
/// point outside the project.
pub fn from_text(text: &str) -> Vec<Bookmark> {
    serde_json::from_str::<Vec<Bookmark>>(text)
        .ok()
        .map(|mut list| {
            list.retain(|b: &Bookmark| clew_core::statefile::safe_rel(&b.rel));
            list
        })
        .unwrap_or_default()
}

/// Encode for persistence; `None` means "delete the store file" (no
/// bookmarks left — `.clew/` itself stays, it records consent).
pub fn to_text(bookmarks: &[Bookmark]) -> Option<String> {
    if bookmarks.is_empty() {
        return None;
    }
    serde_json::to_string_pretty(bookmarks).ok()
}

pub fn load(root: &Path) -> Vec<Bookmark> {
    // Repo-shipped state: guarded read (plain file only, bounded).
    clew_core::statefile::read(&store_path(root))
        .map(|s| from_text(&s))
        .unwrap_or_default()
}

/// Write the list wholesale. Correct only when the caller's list IS the whole
/// truth (a fresh read it has not shared); mutations from a window's long-held
/// snapshot must go through [`edit`] instead.
pub fn save(root: &Path, bookmarks: &[Bookmark]) -> std::io::Result<()> {
    let path = store_path(root);
    match to_text(bookmarks) {
        None => clew_core::statefile::remove(&path),
        Some(json) => clew_core::statefile::write_atomic(&path, json.as_bytes()),
    }
}

/// Serializes the read-modify-write below across this process's windows.
///
/// Every window owns its own `App`, so each holds the `bookmarks` snapshot it
/// loaded when it opened the project — two windows on one project are two
/// writers with copies that are hours old by the time they save.
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Apply one change to what is on disk RIGHT NOW and persist it, returning the
/// merged list.
///
/// The re-read is the whole point. `save` writes a window's in-memory Vec
/// wholesale, so the last window to bookmark a line erased every bookmark the
/// other window had added since it opened the project — and because the losing
/// window kept rendering its own stale copy, nothing looked wrong until the
/// next launch. Callers must adopt the returned list for exactly that reason,
/// and must address entries by identity (`rel` + `line`), never by an index
/// into their own snapshot, which the merge reorders.
///
/// Two clew PROCESSES on one project are covered too, by the file lock the
/// read-modify-write is wrapped in — the in-process `Mutex` alone is invisible
/// to a second launch of the app (or a dev build beside a release one), and
/// both would read the same list and let the later `rename` win.
///
/// Residual, accepted: that lock is best effort. On a `.clew/` it cannot
/// create the lock file in (a read-only checkout) or a filesystem without
/// `flock`, [`clew_core::statefile::lock_exclusive`] returns `None` and this
/// runs unlocked, exactly as it did before — two processes can then still
/// interleave within the milliseconds between the read and the rename, losing
/// one entry. Never a torn file, which the atomic write rules out.
///
/// The merged list is returned even when the write FAILED, which is why this
/// is a tuple and not a `Result<Vec<Bookmark>>`. `on_bookmark_note_save` has
/// already taken the user's draft by the time it calls this, so dropping the
/// merged list on an unwritable `.clew/` destroyed the note text outright.
/// Adopting it is safe: the read succeeded, only the write did not, so it is
/// disk-plus-this-change, and the caller says it is unsaved.
pub fn edit(
    root: &Path,
    change: impl FnOnce(&mut Vec<Bookmark>),
) -> (Vec<Bookmark>, std::io::Result<()>) {
    // Poisoning only means an earlier caller panicked; the list is re-read from
    // disk here regardless, so there is no corrupt state to inherit.
    let _serialized = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Held across the read AND the rename below: this is the half the
    // in-process lock cannot do, and it is what a second clew process
    // contends on.
    let _exclusive = clew_core::statefile::lock_exclusive(&store_path(root));
    let mut merged = load(root);
    change(&mut merged);
    let saved = save(root, &merged);
    (merged, saved)
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
        sort(list);
        true
    }
}

/// The stored order: by file, then by line.
///
/// Applied to what comes back from a REMOTE merge as well. The server appends
/// a new entry rather than inserting it in order — it is given the fields that
/// identify an entry, not the ones the store sorts on — so the display order
/// is re-established here.
pub fn sort(list: &mut [Bookmark]) {
    list.sort_by(|a, b| a.rel.cmp(&b.rel).then(a.line.cmp(&b.line)));
}

/// The state file this store lives in, as the protocol addresses it.
pub const REL: &str = "bookmarks.json";

/// A bookmark's identity on the wire: the file and the line, never an index
/// into the caller's snapshot (which another client's insert reorders).
fn key(rel: &str, line: usize) -> (Vec<String>, Vec<serde_json::Value>) {
    (
        vec!["rel".into(), "line".into()],
        vec![rel.into(), line.into()],
    )
}

fn merge(rel: &str, line: usize, edit: clew_protocol::StateEdit) -> clew_protocol::StateMerge {
    let (key_fields, key) = key(rel, line);
    clew_protocol::StateMerge {
        key_fields,
        key,
        edit,
        // An empty list has no file (see `to_text`).
        delete_when_empty: true,
    }
}

/// The remote twin of [`toggle`]: the SERVER decides whether this is an add or
/// a removal, by looking at the file. Whether the bookmark is already there is
/// exactly what a client's session-old snapshot gets wrong.
pub fn merge_toggle(rel: &str, line: usize, preview: String) -> clew_protocol::StateMerge {
    let entry = serde_json::json!({
        "rel": rel, "line": line, "preview": preview, "note": serde_json::Value::Null,
    });
    merge(rel, line, clew_protocol::StateEdit::Toggle(entry))
}

/// Remove the bookmark at `rel:line`, by identity.
pub fn merge_remove(rel: &str, line: usize) -> clew_protocol::StateMerge {
    merge(rel, line, clew_protocol::StateEdit::Remove)
}

/// The remote twin of [`set_note`]: patch the note field of the entry on
/// disk, leaving every other field (and every other entry) as the file has
/// them. `insert: None` mirrors `set_note`'s no-op when the bookmark is gone —
/// recreating it from a stale snapshot would resurrect a bookmark another
/// client deleted.
pub fn merge_note(rel: &str, line: usize, note: Option<String>) -> clew_protocol::StateMerge {
    let note = note.filter(|s| !s.trim().is_empty());
    let mut fields = serde_json::Map::new();
    fields.insert("note".into(), serde_json::json!(note));
    merge(
        rel,
        line,
        clew_protocol::StateEdit::Patch {
            fields,
            insert: None,
            // A bookmark with no note is still a bookmark.
            empty_when: Vec::new(),
        },
    )
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

    fn bm(rel: &str, line: usize) -> Bookmark {
        Bookmark {
            rel: rel.into(),
            line,
            preview: String::new(),
            note: None,
        }
    }

    /// Two windows on one project. The second window's save must not erase the
    /// bookmark the first added after the second loaded its snapshot — the
    /// whole-file write did exactly that, silently, until the next launch.
    #[test]
    fn edit_keeps_the_other_windows_bookmark() {
        let root = std::env::temp_dir().join("clew-bm-two-windows-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        save(&root, &[bm("a.rs", 1)]).unwrap();

        // Both windows open the project and snapshot [a.rs:1].
        let window2 = load(&root);
        assert_eq!(window2.len(), 1);
        // Window 1 bookmarks b.rs:2 while window 2 sits on its snapshot.
        edit(&root, |list| {
            toggle(list, "b.rs", 2, "one".into());
        })
        .1
        .unwrap();

        // What the whole-file write did: window 2 bookmarks from its snapshot
        // and b.rs:2 is gone from disk, with nothing to tell it so.
        let mut stale = window2.clone();
        toggle(&mut stale, "c.rs", 3, "two".into());
        save(&root, &stale).unwrap();
        assert!(!load(&root).iter().any(|b| b.rel == "b.rs"));

        // Same edit through `edit`: window 1's bookmark survives.
        save(&root, &[bm("a.rs", 1), bm("b.rs", 2)]).unwrap();
        let (merged, saved) = edit(&root, |list| {
            toggle(list, "c.rs", 3, "two".into());
        });
        saved.unwrap();

        let on_disk = load(&root);
        assert_eq!(merged, on_disk, "the caller adopts what was written");
        let lines: Vec<_> = on_disk.iter().map(|b| b.rel.as_str()).collect();
        assert_eq!(lines, ["a.rs", "b.rs", "c.rs"]);
    }

    /// Removal addresses a bookmark by identity, so it deletes the entry the
    /// user clicked even though the merge shifted every index in the window's
    /// snapshot.
    #[test]
    fn edit_removes_by_identity_not_by_stale_index() {
        let root = std::env::temp_dir().join("clew-bm-stale-index-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        save(&root, &[bm("m.rs", 1)]).unwrap();

        // This window's snapshot: m.rs:1 sits at index 0.
        let window = load(&root);
        let target = window[0].clone();
        // Another window adds a bookmark that sorts first, so index 0 on disk
        // is now a different entry.
        edit(&root, |list| {
            toggle(list, "a.rs", 9, "other".into());
        })
        .1
        .unwrap();

        let (merged, saved) = edit(&root, |list| {
            list.retain(|b| !(b.rel == target.rel && b.line == target.line))
        });
        saved.unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!((merged[0].rel.as_str(), merged[0].line), ("a.rs", 9));
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

    /// A failed write must still return the merged list. `on_bookmark_note_save`
    /// has already taken the user's draft by then, so an `Err`-only return
    /// deleted the note text it was carrying.
    #[test]
    fn edit_returns_the_note_when_the_write_fails() {
        let root = std::env::temp_dir().join("clew-bm-unwritable-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // `.clew` as a plain file: `write_atomic` refuses every state write
        // under it, the same shape a read-only checkout produces.
        std::fs::write(root.join(".clew"), "not a dir").unwrap();

        let (merged, saved) = edit(&root, |list| {
            toggle(list, "a.rs", 1, "fn main() {".into());
            set_note(list, "a.rs", 1, Some("typed prose".into()));
        });
        assert!(saved.is_err(), "the store is unwritable");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].note.as_deref(), Some("typed prose"));
    }
}
