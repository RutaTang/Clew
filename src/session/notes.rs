//! Per-project reading notes and progress.
//!
//! A reading note is anchored to a SYMBOL (file + symbol name), never a raw
//! line. That is what makes it survive edits and re-scans: the current line is
//! resolved from the live symbol index at display time, so there is nothing to
//! migrate on refresh, and a note whose symbol has vanished (renamed/deleted)
//! surfaces as detached rather than silently pointing at the wrong code.
//!
//! Persisted with the project in `<root>/.clew/notes.json` (atomic write), so
//! notes can travel with it (the file is deliberately NOT in the `.gitignore`
//! the state layer writes). An emptied list removes its own file, never
//! `.clew/`.
//!
//! Same rules as the bookmarks store, and the same implementation
//! (`session::store`): a file that exists but cannot be read or understood is
//! reported by [`load_checked`] and never overwritten by [`edit_with_fallback`]
//! (nor by the test-only `save` and `edit`); the layout is
//! `clew_core::statefile`'s JSON-array store (bare array = schema 1, a newer
//! schema is never written over); unknown entry fields are ignored on read and
//! kept on write, as is the layout.

use std::path::Path;

use clew_core::statefile::StoreError;
use serde::{Deserialize, Serialize};

use super::store;

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

impl store::Entry for Note {
    const FILE: &'static str = REL;

    fn rel(&self) -> &str {
        &self.rel
    }

    /// A note is its file and symbol.
    type Key = (String, String);

    fn key(&self) -> Self::Key {
        (self.rel.clone(), self.symbol.clone())
    }
}

/// Decode a store file's text (shared by the local disk path and the remote
/// protocol path). Rel paths validated: a crafted entry must not point
/// outside the project (such entries are hidden, not an error).
pub fn try_from_text(text: &str) -> Result<Vec<Note>, StoreError> {
    store::try_from_text(text)
}

/// [`try_from_text`] for DISPLAY: a store that cannot be understood shows as
/// empty. Never use this for text that will be written back.
pub fn from_text(text: &str) -> Vec<Note> {
    try_from_text(text).unwrap_or_default()
}

/// The notes on disk: empty when there is no store yet, an error when there
/// is one that cannot be read or understood.
pub fn load_checked(root: &Path) -> Result<Vec<Note>, StoreError> {
    store::load_checked(root)
}

/// [`load_checked`] for DISPLAY: an unreadable store shows as empty.
#[cfg(test)]
pub fn load(root: &Path) -> Vec<Note> {
    load_checked(root).unwrap_or_default()
}

/// Persist the notes (atomic temp+rename). An empty list removes the file.
/// Correct only when the caller's list IS the whole truth; a change made from
/// a window's long-held snapshot must go through [`edit`] — which is why
/// nothing but the tests uses this. Refuses — without touching the file —
/// when what is on disk cannot be read or understood.
#[cfg(test)]
pub fn save(root: &Path, notes: &[Note]) -> std::io::Result<()> {
    edit(root, |list| *list = notes.to_vec()).1
}

/// Apply one change to what is on disk RIGHT NOW and persist it, returning the
/// merged list the caller must adopt.
///
/// The re-read is the whole point. `save` writes a window's in-memory Vec
/// wholesale, so the last window to annotate a symbol erased every note the
/// other window had written since it opened the project — invisibly, because
/// each window kept rendering its own copy until the next launch.
///
/// The change has to be applied HERE rather than merged afterwards: a note
/// missing from a window's snapshot is either one it just deleted or one the
/// other window just added, and nothing in the two lists tells those apart.
/// Replaying the mutation (`set_understood` / `set_text` / `remove`) onto
/// the freshly-read list keeps both windows' notes AND still deletes.
///
/// Every such mutation must therefore be expressed as a VALUE, never as a
/// flip: what it is replayed against is by definition the state the caller
/// could not see, so "the opposite of what is there" is the opposite of what
/// the user asked for.
///
/// Two clew PROCESSES are covered too, by the file lock this is wrapped in;
/// the in-process `Mutex` alone is invisible to a second launch of the app.
/// The lock follows `statefile::lock`'s one policy: a filesystem that cannot
/// lock at all runs unlocked (a milliseconds-wide residual race; the write
/// stays atomic), any other failure to lock refuses the write.
///
/// **Nothing is written over a store that cannot be read or understood** —
/// the result is then this change applied to an empty list, with an error
/// saying the store was left alone. [`edit_with_fallback`] applies it to the
/// window's own snapshot instead.
///
/// The merged list is returned even when the write FAILED, which is why this
/// is a tuple and not a `Result<Vec<Note>>`. Callers have already consumed
/// what the user typed (`NoteEditSave` takes the draft before calling), so
/// dropping the merged list on an unwritable `.clew/` destroyed prose that
/// existed nowhere else — the note stays on screen for the session, and the
/// caller reports that it is unsaved.
#[cfg(test)]
pub fn edit(root: &Path, change: impl FnOnce(&mut Vec<Note>)) -> (Vec<Note>, std::io::Result<()>) {
    edit_with_fallback(root, &[], change)
}

/// `edit` (test-only), with `fallback` — normally the caller's own snapshot —
/// as the list the change is applied to when the store on disk cannot be read,
/// understood or locked. Nothing is written in that case either way.
pub fn edit_with_fallback(
    root: &Path,
    fallback: &[Note],
    change: impl FnOnce(&mut Vec<Note>),
) -> (Vec<Note>, std::io::Result<()>) {
    store::edit_with_fallback(root, fallback, change)
}

/// The note for `(rel, symbol)`, if any.
pub fn find<'a>(list: &'a [Note], rel: &str, symbol: &str) -> Option<&'a Note> {
    list.iter().find(|n| n.rel == rel && n.symbol == symbol)
}

/// The stored order: by file, then by symbol. Also applied to what comes back
/// from a REMOTE merge, which appends rather than inserting in order (the
/// server is given the fields that identify a note, not the ones it sorts on).
pub fn sort(list: &mut [Note]) {
    list.sort_by(|a, b| a.rel.cmp(&b.rel).then_with(|| a.symbol.cmp(&b.symbol)));
}

/// The state file this store lives in, as the protocol addresses it.
pub const REL: &str = "notes.json";

/// The fields a note is addressed by on the wire.
fn merge(rel: &str, symbol: &str, edit: clew_protocol::StateEdit) -> clew_protocol::StateMerge {
    clew_protocol::StateMerge {
        key_fields: vec!["rel".into(), "symbol".into()],
        key: vec![rel.into(), symbol.into()],
        edit,
        // An empty list has no file, as with every clew store.
        delete_when_empty: true,
    }
}

/// A note that carries neither the flag nor any text is dropped by the merge,
/// the same rule [`Note::is_empty`] applies locally. The server is told which
/// fields carry the information; it does not know what a note is.
const CARRIES_INFORMATION: [&str; 2] = ["understood", "text"];

/// The remote twin of [`set_understood`], with `understood` resolved by the
/// caller: the user is flipping the flag they can SEE, so their intent is a
/// value, not "the opposite of whatever is on disk". Every other field of the
/// note — the prose another client may have written meanwhile — is left as the
/// file has it.
pub fn merge_understood(rel: &str, symbol: &str, understood: bool) -> clew_protocol::StateMerge {
    let mut fields = serde_json::Map::new();
    fields.insert("understood".into(), understood.into());
    merge(
        rel,
        symbol,
        clew_protocol::StateEdit::Patch {
            fields,
            insert: Some(serde_json::json!({
                "rel": rel, "symbol": symbol, "understood": understood, "text": "",
            })),
            empty_when: CARRIES_INFORMATION.map(String::from).to_vec(),
        },
    )
}

/// The remote twin of [`set_text`]: patch just the prose, so a concurrent
/// "understood" flip from another client survives it.
pub fn merge_text(rel: &str, symbol: &str, text: &str) -> clew_protocol::StateMerge {
    let text = text.trim();
    let mut fields = serde_json::Map::new();
    fields.insert("text".into(), text.into());
    merge(
        rel,
        symbol,
        clew_protocol::StateEdit::Patch {
            fields,
            insert: Some(serde_json::json!({
                "rel": rel, "symbol": symbol, "understood": false, "text": text,
            })),
            empty_when: CARRIES_INFORMATION.map(String::from).to_vec(),
        },
    )
}

/// The remote twin of [`remove`].
pub fn merge_remove(rel: &str, symbol: &str) -> clew_protocol::StateMerge {
    merge(rel, symbol, clew_protocol::StateEdit::Remove)
}

/// Set the "understood" flag for `(rel, symbol)` to `understood`, creating the
/// note when it is being marked and dropping it if it becomes empty.
///
/// A VALUE, not a flip, and for the same reason as [`merge_understood`]: this
/// runs inside [`edit_with_fallback`], i.e. replayed on the list just read
/// from disk, which is precisely the state the calling window cannot see. A
/// flip replayed there lands on the opposite of what the reader clicked — a
/// second window that had already marked the symbol turns it back off,
/// deleting its note entry, while the window that clicked shows no change at
/// all.
///
/// The three cases mirror the remote `Patch` exactly: assign the field, drop
/// the entry once neither carrier holds anything, and do NOT resurrect a note
/// that is already gone when clearing the flag.
pub fn set_understood(list: &mut Vec<Note>, rel: &str, symbol: &str, understood: bool) {
    match list.iter().position(|n| n.rel == rel && n.symbol == symbol) {
        Some(pos) => {
            list[pos].understood = understood;
            if list[pos].is_empty() {
                list.remove(pos);
            }
        }
        None if understood => {
            list.push(Note {
                rel: rel.into(),
                symbol: symbol.into(),
                understood: true,
                text: String::new(),
            });
            sort(list);
        }
        None => {}
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
    // The outline shows this on every repaint: index the file's notes once
    // (first note per name, as `find` resolves it) rather than scanning the
    // whole list per symbol.
    let mut by_symbol: std::collections::HashMap<&str, &Note> = std::collections::HashMap::new();
    for n in list.iter().filter(|n| n.rel == rel) {
        by_symbol.entry(n.symbol.as_str()).or_insert(n);
    }
    let understood = symbols
        .iter()
        .filter(|name| by_symbol.get(name.as_str()).is_some_and(|n| n.understood))
        .count();
    (understood, symbols.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clew_core::testutil::TempDir;

    #[test]
    fn set_understood_creates_assigns_and_drops() {
        let mut list = Vec::new();
        set_understood(&mut list, "a.rs", "foo", true);
        assert_eq!(list.len(), 1);
        // Clearing it with no text drops the now-empty note.
        set_understood(&mut list, "a.rs", "foo", false);
        assert!(list.is_empty());
        // Idempotent both ways, and clearing an absent note resurrects nothing
        // — the same rule the remote `Patch` follows with `insert: None`.
        set_understood(&mut list, "a.rs", "foo", false);
        assert!(list.is_empty());
        set_understood(&mut list, "a.rs", "foo", true);
        set_understood(&mut list, "a.rs", "foo", true);
        assert_eq!(list.len(), 1);
        assert!(find(&list, "a.rs", "foo").unwrap().understood);
    }

    #[test]
    fn text_kept_when_unmarking_understood() {
        let mut list = Vec::new();
        set_understood(&mut list, "a.rs", "foo", true);
        set_text(&mut list, "a.rs", "foo", "  a note  ");
        assert_eq!(find(&list, "a.rs", "foo").unwrap().text, "a note");
        // Un-understanding keeps the note because it still has text.
        set_understood(&mut list, "a.rs", "foo", false);
        assert_eq!(list.len(), 1);
        // Clearing the text now drops it (no flag, no text).
        set_text(&mut list, "a.rs", "foo", "");
        assert!(list.is_empty());
    }

    /// The reason this is a value and not a flip: `edit` replays the change on
    /// what is on disk RIGHT NOW, and a second window has already flipped it
    /// there. Replaying a toggle inverted the reader's click and deleted the
    /// other window's flag; replaying the resolved value lands it.
    #[test]
    fn edit_lands_the_flag_the_reader_chose_over_a_changed_file() {
        let root = TempDir::new("notes-understood-stale");

        // Window 2's snapshot from project open: nothing marked.
        let window2: Vec<Note> = load(&root);
        assert!(window2.is_empty());
        // Window 1 marks the symbol understood in the meantime.
        edit(&root, |list| set_understood(list, "a.rs", "bar", true))
            .1
            .unwrap();

        // Window 2 clicks the checkbox it still renders as UNCHECKED, so the
        // value it resolved is `true` — already true on disk, hence a no-op.
        let want = !find(&window2, "a.rs", "bar").is_some_and(|n| n.understood);
        assert!(want);
        let (merged, saved) = edit(&root, |list| set_understood(list, "a.rs", "bar", want));
        saved.unwrap();
        assert!(
            find(&merged, "a.rs", "bar").is_some_and(|n| n.understood),
            "the flag the reader asked for, not the opposite of the file"
        );
        assert_eq!(load(&root), merged);

        // And unmarking still unmarks: window 2 now sees it checked.
        let want = !find(&merged, "a.rs", "bar").is_some_and(|n| n.understood);
        assert!(!want);
        let (merged, saved) = edit(&root, |list| set_understood(list, "a.rs", "bar", want));
        saved.unwrap();
        assert!(merged.is_empty());
    }

    #[test]
    fn coverage_counts_only_live_symbols() {
        let mut list = Vec::new();
        set_understood(&mut list, "a.rs", "foo", true);
        set_understood(&mut list, "a.rs", "gone", true); // symbol later deleted
        // The live index only has `foo` and `bar`; `gone` is not counted.
        let syms = vec!["foo".to_string(), "bar".to_string()];
        assert_eq!(coverage(&list, "a.rs", &syms), (1, 2));
    }

    #[test]
    fn roundtrips_through_disk() {
        let root = TempDir::new("notes-roundtrip");
        let mut list = Vec::new();
        set_understood(&mut list, "src/main.rs", "main", true);
        set_text(&mut list, "src/main.rs", "main", "entry point");
        save(&root, &list).unwrap();
        assert!(root.join(".clew/notes.json").exists());
        assert_eq!(load(&root), list);
        // Emptying removes the file but keeps .clew/.
        save(&root, &[]).unwrap();
        assert!(!root.join(".clew/notes.json").exists());
        assert!(root.join(".clew").is_dir());
    }

    /// Two windows on one project: the second window's save must not erase the
    /// note the first wrote after the second loaded its snapshot — the
    /// whole-file write did exactly that, silently, until the next launch.
    #[test]
    fn edit_keeps_the_other_windows_note() {
        let root = TempDir::new("notes-two-windows");
        let mut seed = Vec::new();
        set_text(&mut seed, "a.rs", "alpha", "first");
        save(&root, &seed).unwrap();

        // Both windows open the project and snapshot the one note.
        let window2 = load(&root);
        assert_eq!(window2.len(), 1);
        // Window 1 annotates another symbol while window 2 holds its snapshot.
        edit(&root, |list| set_text(list, "b.rs", "beta", "second"))
            .1
            .unwrap();

        // What the whole-file write did: window 2 saves its snapshot and the
        // note window 1 just wrote is gone, with nothing to tell either window.
        let mut stale = window2.clone();
        set_text(&mut stale, "c.rs", "gamma", "third");
        save(&root, &stale).unwrap();
        assert!(!load(&root).iter().any(|n| n.symbol == "beta"));

        // Same edit through `edit`: window 1's note survives.
        save(&root, &[]).unwrap();
        let mut seeded = Vec::new();
        set_text(&mut seeded, "a.rs", "alpha", "first");
        set_text(&mut seeded, "b.rs", "beta", "second");
        save(&root, &seeded).unwrap();
        let (merged, saved) = edit(&root, |list| set_text(list, "c.rs", "gamma", "third"));
        saved.unwrap();

        let on_disk = load(&root);
        assert_eq!(merged, on_disk, "the caller adopts what was written");
        let symbols: Vec<_> = on_disk.iter().map(|n| n.symbol.as_str()).collect();
        assert_eq!(symbols, ["alpha", "beta", "gamma"]);
    }

    /// Replaying the mutation inside `edit` is what lets a delete still delete:
    /// merging the two lists afterwards cannot tell a note this window removed
    /// from one the other window just added.
    #[test]
    fn edit_deletes_while_keeping_the_other_windows_note() {
        let root = TempDir::new("notes-two-windows-delete");
        let mut seed = Vec::new();
        set_text(&mut seed, "a.rs", "alpha", "first");
        save(&root, &seed).unwrap();

        let window2 = load(&root);
        assert_eq!(window2.len(), 1);
        edit(&root, |list| set_text(list, "b.rs", "beta", "second"))
            .1
            .unwrap();
        let (merged, saved) = edit(&root, |list| remove(list, "a.rs", "alpha"));
        saved.unwrap();

        let symbols: Vec<_> = merged.iter().map(|n| n.symbol.as_str()).collect();
        assert_eq!(symbols, ["beta"]);
        assert_eq!(load(&root), merged);
    }

    /// A failed write must still return the merged list. The caller has
    /// already consumed the user's draft, so an `Err`-only return deleted the
    /// note the instant it could not be persisted.
    #[test]
    fn edit_returns_the_note_when_the_write_fails() {
        let root = TempDir::new("notes-unwritable");
        // `.clew` as a plain file: `write_atomic` refuses every state write
        // under it, the same shape a read-only checkout produces.
        std::fs::write(root.join(".clew"), "not a dir").unwrap();

        let (merged, saved) = edit(&root, |list| set_text(list, "a.rs", "alpha", "typed prose"));
        assert!(saved.is_err(), "the store is unwritable");
        assert_eq!(
            find(&merged, "a.rs", "alpha").map(|n| n.text.as_str()),
            Some("typed prose"),
            "the caller can still keep the note for the session"
        );
    }

    /// Prose another reader wrote must not be replaced because clew could
    /// not parse the file it sits in: every write path refuses, the fallback
    /// keeps this window's notes on screen, and the bytes survive.
    #[test]
    fn an_unreadable_notes_store_is_never_overwritten() {
        let root = TempDir::new("notes-unparseable");
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        let path = root.join(".clew/notes.json");
        let hand_edited = "[{\"rel\":\"a.rs\",\"symbol\":\"f\",\"text\":\"precious prose\"},]";
        std::fs::write(&path, hand_edited).unwrap();

        assert!(load_checked(&root).is_err(), "a trailing comma is not JSON");
        let mine = vec![Note {
            rel: "b.rs".into(),
            symbol: "g".into(),
            understood: true,
            text: String::new(),
        }];
        let (merged, saved) =
            edit_with_fallback(&root, &mine, |list| set_text(list, "c.rs", "h", "new"));
        assert!(saved.is_err());
        let symbols: Vec<&str> = merged.iter().map(|n| n.symbol.as_str()).collect();
        assert_eq!(
            symbols,
            ["g", "h"],
            "the window keeps what it had, plus the edit"
        );
        assert!(save(&root, &merged).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), hand_edited);

        // Not UTF-8 is refused the same way (read, not parse).
        std::fs::write(&path, b"[\xff]").unwrap();
        assert!(matches!(load_checked(&root), Err(StoreError::Refused(_))));
        assert!(edit(&root, |list| remove(list, "a.rs", "f")).1.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"[\xff]");
    }
}
