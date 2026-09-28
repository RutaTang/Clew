//! Per-project bookmarks.
//!
//! Persisted with the project in `<root>/.clew/bookmarks.json`, so they can
//! travel with it (the file is deliberately NOT in the `.gitignore` the state
//! layer writes). An emptied store removes its own file, never `.clew/`.
//!
//! **A store clew cannot read is never overwritten.** A file that exists but
//! was refused (too large, not a plain file, not UTF-8, unreadable) or does
//! not parse (a hand edit, a git conflict marker, a newer clew's layout) used
//! to load as an empty list — and the next bookmark then replaced the whole
//! file with one entry. Now [`load_checked`] reports it, and every write path
//! ([`edit_with_fallback`], and the test-only `save` and `edit`) refuses with
//! an error the caller shows, leaving the bytes for the user to fix.
//!
//! **Schema.** The layout is `clew_core::statefile`'s JSON-array store: a
//! bare array is schema 1, which is what every clew version reads and what is
//! written today; a `{"schema_version": N, "entries": [...]}` envelope is
//! understood, and one from a newer schema is shown nowhere and written never
//! (see `statefile::ARRAY_STORE_SCHEMA`). Unknown fields in an entry are
//! ignored on read — so an older clew keeps working on a newer entry shape —
//! and KEPT on write, together with the layout the file was in: a local edit
//! is written back onto the values it was read from (see `session::store`),
//! the same as the remote merge.

use std::path::Path;

use clew_core::statefile::StoreError;
use serde::{Deserialize, Serialize};

use super::store;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub rel: String,
    pub line: usize, // 1-based
    pub preview: String,
    /// Optional freeform (plain-text) note the reader attached.
    #[serde(default)]
    pub note: Option<String>,
}

impl store::Entry for Bookmark {
    const FILE: &'static str = REL;

    fn rel(&self) -> &str {
        &self.rel
    }

    /// A bookmark is its file and line.
    type Key = (String, usize);

    fn key(&self) -> Self::Key {
        (self.rel.clone(), self.line)
    }
}

/// Decode a store file's text. Shared by the local disk path and the remote
/// protocol path (`StateContent`); rel paths are validated either way — the
/// text is repo-shipped (or remote-supplied) and a crafted entry must not
/// point outside the project (such entries are hidden, not an error).
pub fn try_from_text(text: &str) -> Result<Vec<Bookmark>, StoreError> {
    store::try_from_text(text)
}

/// [`try_from_text`] for DISPLAY: a store that cannot be understood shows as
/// empty. Never use this for text that will be written back.
pub fn from_text(text: &str) -> Vec<Bookmark> {
    try_from_text(text).unwrap_or_default()
}

/// The bookmarks on disk: empty when there is no store yet, an error when
/// there is one that cannot be read or understood (which callers should show,
/// and which every write path refuses to overwrite).
pub fn load_checked(root: &Path) -> Result<Vec<Bookmark>, StoreError> {
    store::load_checked(root)
}

/// [`load_checked`] for DISPLAY: an unreadable store shows as empty.
#[cfg(test)]
pub fn load(root: &Path) -> Vec<Bookmark> {
    load_checked(root).unwrap_or_default()
}

/// Write the list wholesale. Correct only when the caller's list IS the whole
/// truth (a fresh read it has not shared); mutations from a window's long-held
/// snapshot must go through [`edit`] instead — which is why nothing but the
/// tests uses this. Refuses — without touching the file — when what is on
/// disk cannot be read or understood.
#[cfg(test)]
pub fn save(root: &Path, bookmarks: &[Bookmark]) -> std::io::Result<()> {
    edit(root, |list| *list = bookmarks.to_vec()).1
}

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
/// both would read the same list and let the later `rename` win. The lock
/// follows `statefile::lock`'s one policy: a filesystem that cannot lock at
/// all runs unlocked (the residual race is milliseconds wide, and the atomic
/// write still rules out a torn file); any other failure to lock refuses the
/// write.
///
/// **Nothing is written over a store that cannot be read or understood** —
/// the result is then this change applied to an empty list, with an error
/// saying the store was left alone. Use [`edit_with_fallback`] to apply it to
/// the window's own snapshot instead, so the other bookmarks stay on screen.
///
/// The merged list is returned even when the write FAILED, which is why this
/// is a tuple and not a `Result<Vec<Bookmark>>`. `on_bookmark_note_save` has
/// already taken the user's draft by the time it calls this, so dropping the
/// merged list on an unwritable `.clew/` destroyed the note text outright.
/// Adopting it is safe: it is what the user did, and the caller says it is
/// unsaved.
#[cfg(test)]
pub fn edit(
    root: &Path,
    change: impl FnOnce(&mut Vec<Bookmark>),
) -> (Vec<Bookmark>, std::io::Result<()>) {
    edit_with_fallback(root, &[], change)
}

/// `edit` (test-only), with `fallback` — normally the caller's own snapshot —
/// as the list the change is applied to when the store on disk cannot be read,
/// understood or locked. Nothing is written in that case either way; this only
/// decides what the caller keeps showing for the session.
pub fn edit_with_fallback(
    root: &Path,
    fallback: &[Bookmark],
    change: impl FnOnce(&mut Vec<Bookmark>),
) -> (Vec<Bookmark>, std::io::Result<()>) {
    store::edit_with_fallback(root, fallback, change)
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
        // An empty list has no file, as with every clew store.
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
    use clew_core::testutil::TempDir;

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
        let root = TempDir::new("bm-project");

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
        let root = TempDir::new("bm-two-windows");
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
        let root = TempDir::new("bm-stale-index");
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
        // Parent chain cannot be created inside a file path: make a file at
        // the would-be root parent to force create_dir_all to fail.
        let scratch = TempDir::new("bm-readonly");
        let base = scratch.join("file");
        std::fs::write(&base, "not a dir").unwrap();
        let root = base.join("nonexistent-parent");

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
        let root = TempDir::new("bm-unwritable");
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

    /// The bug this store's checked reads exist for: a file clew cannot
    /// understand — here a git conflict — loaded as empty, and the next
    /// bookmark replaced every entry in it with one. Now every write path
    /// refuses and the bytes survive.
    #[test]
    fn an_unparseable_store_is_refused_and_left_untouched() {
        let root = TempDir::new("bm-unparseable");
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        let conflicted = "<<<<<<< HEAD\n[{\"rel\":\"a.rs\",\"line\":1,\"preview\":\"\"}]\n\
                          =======\n[]\n>>>>>>> theirs\n";
        let path = root.join(".clew/bookmarks.json");
        std::fs::write(&path, conflicted).unwrap();

        assert!(matches!(
            load_checked(&root),
            Err(StoreError::Unparseable(_))
        ));
        assert!(load(&root).is_empty(), "display falls back to nothing");

        let (merged, saved) = edit(&root, |list| {
            toggle(list, "b.rs", 2, "new".into());
        });
        let err = saved.expect_err("an edit over an unparseable store must refuse");
        assert!(err.to_string().contains("untouched"), "{err}");
        assert_eq!(merged.len(), 1, "the user's own change is kept in memory");
        assert!(save(&root, &merged).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), conflicted);

        // A malformed ENTRY is as unknown as a malformed file.
        std::fs::write(&path, r#"[{"rel":"a.rs","line":"one"}]"#).unwrap();
        assert!(load_checked(&root).is_err());
    }

    /// With a fallback, a refused store leaves the window's own bookmarks on
    /// screen (plus the new one) instead of an empty list.
    #[test]
    fn edit_with_fallback_keeps_the_windows_snapshot() {
        let root = TempDir::new("bm-fallback");
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        std::fs::write(root.join(".clew/bookmarks.json"), "{ broken").unwrap();
        let snapshot = vec![bm("a.rs", 1), bm("c.rs", 3)];
        let (merged, saved) = edit_with_fallback(&root, &snapshot, |list| {
            toggle(list, "b.rs", 2, String::new());
        });
        assert!(saved.is_err());
        let rels: Vec<&str> = merged.iter().map(|b| b.rel.as_str()).collect();
        assert_eq!(rels, ["a.rs", "b.rs", "c.rs"]);
        assert_eq!(
            std::fs::read_to_string(root.join(".clew/bookmarks.json")).unwrap(),
            "{ broken"
        );
    }

    /// Schema: the versioned envelope this build knows is read; a newer one
    /// is never written over (so a downgrade cannot destroy it).
    #[test]
    fn a_newer_schema_is_never_overwritten_and_a_known_envelope_loads() {
        let root = TempDir::new("bm-schema");
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        let path = root.join(".clew/bookmarks.json");

        std::fs::write(
            &path,
            r#"{"schema_version":1,"entries":[{"rel":"a.rs","line":4,"preview":"p","future":true}]}"#,
        )
        .unwrap();
        let list = load_checked(&root).expect("a known envelope");
        assert_eq!((list[0].rel.as_str(), list[0].line), ("a.rs", 4));

        let newer = r#"{"schema_version":2,"entries":[],"moved":"elsewhere"}"#;
        std::fs::write(&path, newer).unwrap();
        assert!(matches!(
            load_checked(&root),
            Err(StoreError::NewerSchema { found: 2, .. })
        ));
        let (_, saved) = edit(&root, |list| {
            toggle(list, "b.rs", 1, String::new());
        });
        assert!(saved.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
    }

    /// Bookmarks travel with the project: the ignore file the first write
    /// leaves in `.clew/` does not hide them.
    #[test]
    fn saving_leaves_a_gitignore_that_keeps_bookmarks_trackable() {
        let root = TempDir::new("bm-gitignore");
        edit(&root, |list| {
            toggle(list, "a.rs", 1, String::new());
        })
        .1
        .unwrap();
        let ignore = std::fs::read_to_string(root.join(".clew/.gitignore")).unwrap();
        assert!(ignore.lines().any(|l| l == "*.lock"));
        assert!(!ignore.lines().any(|l| l.contains("bookmarks")));
    }
}
