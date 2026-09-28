//! The keyed JSON-array stores a project keeps in `.clew/` — bookmarks and
//! reading notes — in one implementation: decoding, the guarded load, and the
//! locked read-modify-write the two stores used to carry a copy each of.
//!
//! **What this build does not know is kept.** A store travels with the
//! project and is read by every clew version a team has, so an entry written
//! by a newer one may carry fields this one has no name for, and the file may
//! be the versioned envelope rather than a bare array (see
//! `clew_core::statefile::ARRAY_STORE_SCHEMA`). Edits used to round-trip
//! through the typed entry: every unknown field was dropped and an envelope
//! came back as a bare array — while the REMOTE path (`statefile::merge_file`,
//! which edits the JSON values themselves) kept both. Now a local edit is
//! written back onto the values it was read from ([`reconcile`]): an entry
//! the edit did not touch is written back byte-for-byte as it was read, an
//! edited one keeps its unknown fields, the layout is kept, and entries the
//! decoder hid (a path that would leave the project) are kept too.
//!
//! **A store clew cannot read is never overwritten** (the same rule as every
//! store): a file that exists but was refused or does not parse is an error
//! the caller shows, and nothing is written over it.

use std::path::{Path, PathBuf};

use clew_core::statefile::{ArrayStore, StoreError};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// One kind of entry in a keyed JSON-array store.
pub(crate) trait Entry: Serialize + DeserializeOwned + Clone + PartialEq {
    /// The store's file name under `.clew/` (also its protocol `rel`).
    const FILE: &'static str;

    /// The project file the entry points at; an entry whose path would leave
    /// the project is not shown (and never followed).
    fn rel(&self) -> &str;

    /// The entry's identity — what a merge addresses it by, never an index
    /// into someone's snapshot. An edited entry keeps its key.
    type Key: Eq + std::hash::Hash;

    fn key(&self) -> Self::Key;
}

fn store_path<T: Entry>(root: &Path) -> PathBuf {
    root.join(".clew").join(T::FILE)
}

/// Decode a store file's text (shared by the local disk path and the remote
/// protocol path). An entry of the wrong shape makes the whole store an error
/// — it is not a file this build understands — while an entry whose path
/// would leave the project is merely hidden: the text is repo-shipped (or
/// remote-supplied), and a crafted entry must not point outside the project.
pub(crate) fn try_from_text<T: Entry>(text: &str) -> Result<Vec<T>, StoreError> {
    let store = clew_core::statefile::parse_array_store(text)?;
    Ok(decode::<T>(&store)?
        .into_iter()
        .filter_map(|(entry, _)| entry)
        .collect())
}

/// Each value of `store` with its decoded entry — `None` for one that is
/// hidden (its path would leave the project) — and its index in the file.
fn decode<T: Entry>(store: &ArrayStore) -> Result<Vec<(Option<T>, usize)>, StoreError> {
    store
        .entries
        .iter()
        .enumerate()
        .map(|(i, value)| {
            let entry = serde_json::from_value::<T>(value.clone())
                .map_err(|e| StoreError::Unparseable(format!("entry {}: {e}", i + 1)))?;
            let shown = clew_core::statefile::safe_rel(entry.rel()).then_some(entry);
            Ok((shown, i))
        })
        .collect()
}

/// The entries on disk: empty when there is no store yet, an error when there
/// is one that cannot be read or understood (which callers should show, and
/// which every write path refuses to overwrite).
pub(crate) fn load_checked<T: Entry>(root: &Path) -> Result<Vec<T>, StoreError> {
    match clew_core::statefile::load_array_store(&store_path::<T>(root))? {
        None => Ok(Vec::new()),
        Some(store) => Ok(decode::<T>(&store)?
            .into_iter()
            .filter_map(|(entry, _)| entry)
            .collect()),
    }
}

/// Serializes the read-modify-write below across this process's windows
/// (every window owns its own `App` and a snapshot of the store). The file
/// lock inside it is what a second clew PROCESS contends on; this covers the
/// filesystems that cannot lock at all.
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Apply `change` to what is on disk RIGHT NOW and persist it, returning the
/// merged list the caller must adopt (see the stores' `edit_with_fallback`).
///
/// When the store cannot be read, understood or locked, nothing is written:
/// the change is applied to `fallback` (normally the caller's own snapshot)
/// so the caller keeps showing something sensible, and the error says the
/// store was left alone. The list is returned even when the WRITE failed —
/// the caller has usually consumed the user's input already.
pub(crate) fn edit_with_fallback<T: Entry>(
    root: &Path,
    fallback: &[T],
    change: impl FnOnce(&mut Vec<T>),
) -> (Vec<T>, std::io::Result<()>) {
    // Poisoning only means an earlier caller panicked; the store is re-read
    // from disk here regardless, so there is no corrupt state to inherit.
    let _serialized = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = store_path::<T>(root);
    // The file lock is held across the read AND the write: the half the
    // in-process lock cannot do.
    let locked_read = clew_core::statefile::lock(&path).and_then(|lock| {
        // A remote edit a crashed server left pending is settled against the
        // store as it is, before this edit replaces it — settled after, it
        // would read as not landed, and its replay would apply it twice.
        clew_core::statefile::settle_pending_edit(&path)?;
        let store = clew_core::statefile::load_array_store(&path)?.unwrap_or(ArrayStore {
            entries: Vec::new(),
            envelope: None,
        });
        let decoded = decode::<T>(&store)?;
        Ok((lock, store, decoded))
    });
    match locked_read {
        Ok((_exclusive, store, decoded)) => {
            let before: Vec<(T, usize)> = decoded
                .iter()
                .filter_map(|(entry, i)| entry.clone().map(|e| (e, *i)))
                .collect();
            let hidden: Vec<usize> = decoded
                .iter()
                .filter(|(entry, _)| entry.is_none())
                .map(|(_, i)| *i)
                .collect();
            let mut merged: Vec<T> = before.iter().map(|(e, _)| e.clone()).collect();
            change(&mut merged);
            let saved = reconcile(&store, &before, &hidden, &merged)
                .and_then(|written| write(&path, written));
            (merged, saved)
        }
        Err(e) => {
            let mut list = fallback.to_vec();
            change(&mut list);
            (list, Err(e))
        }
    }
}

/// The store to write back for `after`, built on the values it was read
/// from (see the module docs): per entry of `after`, the value of an equal
/// entry from `before` verbatim, else the value of the same entry with the
/// known fields replaced, else a new value; then the hidden entries; in the
/// layout the store was read in. `None` when nothing is left (the file goes).
fn reconcile<T: Entry>(
    store: &ArrayStore,
    before: &[(T, usize)],
    hidden: &[usize],
    after: &[T],
) -> std::io::Result<Option<ArrayStore>> {
    // Each key's entries as read, in file order, each usable once. Lookups
    // by key keep this linear: it runs on every note and bookmark edit, and a
    // store can hold thousands of entries.
    let mut by_key: std::collections::HashMap<T::Key, Vec<usize>> =
        std::collections::HashMap::with_capacity(before.len());
    for (k, (entry, _)) in before.iter().enumerate() {
        by_key.entry(entry.key()).or_default().push(k);
    }
    let mut entries = Vec::with_capacity(after.len() + hidden.len());
    for entry in after {
        // An equal entry if there is one (it was not touched), else the
        // first one with the same identity (it was edited).
        let read_as = by_key.get_mut(&entry.key()).and_then(|candidates| {
            let at = candidates
                .iter()
                .position(|&k| before[k].0 == *entry)
                .or((!candidates.is_empty()).then_some(0))?;
            Some(candidates.remove(at))
        });
        let value = match read_as {
            Some(k) => {
                let original = store.entries[before[k].1].clone();
                if before[k].0 == *entry {
                    original
                } else {
                    with_known_fields(original, entry)?
                }
            }
            None => serde_json::to_value(entry).map_err(std::io::Error::other)?,
        };
        entries.push(value);
    }
    entries.extend(hidden.iter().map(|&i| store.entries[i].clone()));
    if entries.is_empty() {
        return Ok(None);
    }
    Ok(Some(ArrayStore {
        entries,
        envelope: store.envelope,
    }))
}

/// `original` with every field `entry` knows replaced by `entry`'s value —
/// the fields it does not know stay as they were.
fn with_known_fields<T: Entry>(
    original: serde_json::Value,
    entry: &T,
) -> std::io::Result<serde_json::Value> {
    let known = serde_json::to_value(entry).map_err(std::io::Error::other)?;
    Ok(match (original, known) {
        (serde_json::Value::Object(mut kept), serde_json::Value::Object(known)) => {
            kept.extend(known);
            serde_json::Value::Object(kept)
        }
        (_, known) => known,
    })
}

/// Persist `store`, or delete the file when there is nothing left.
fn write(path: &Path, store: Option<ArrayStore>) -> std::io::Result<()> {
    match store {
        None => clew_core::statefile::remove(path),
        Some(store) => {
            let text = store
                .to_text()
                .ok_or_else(|| std::io::Error::other("the store could not be serialized"))?;
            clew_core::statefile::write_atomic(path, text.as_bytes())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bookmarks::Bookmark;
    use clew_core::testutil::TempDir;

    fn read(root: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(store_path::<Bookmark>(root)).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// A newer clew's fields and layout survive this one's edits, exactly as
    /// they survive the remote merge: unknown fields of an untouched entry
    /// and of an edited one, the versioned envelope, and a hidden entry.
    #[test]
    fn a_local_edit_keeps_what_this_build_does_not_know() {
        let root = TempDir::new("store-unknown");
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        std::fs::write(
            store_path::<Bookmark>(&root),
            r#"{"schema_version": 1, "entries": [
                {"rel": "a.rs", "line": 1, "preview": "a", "color": "red"},
                {"rel": "b.rs", "line": 2, "preview": "b", "pinned": true},
                {"rel": "../outside.rs", "line": 3, "preview": "x"}
            ]}"#,
        )
        .unwrap();

        let (merged, saved) = edit_with_fallback::<Bookmark>(&root, &[], |list| {
            crate::bookmarks::set_note(list, "b.rs", 2, Some("mine".into()));
            crate::bookmarks::toggle(list, "c.rs", 3, "c".into());
        });
        saved.unwrap();
        assert_eq!(merged.len(), 3, "the hidden entry is not shown");

        let written = read(&root);
        assert_eq!(written["schema_version"], 1, "the envelope is kept");
        let entries = written["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0]["color"], "red", "an untouched entry, verbatim");
        assert_eq!(
            entries[1]["pinned"], true,
            "an edited entry keeps its fields"
        );
        assert_eq!(entries[1]["note"], "mine");
        assert_eq!(entries[2]["rel"], "c.rs", "the new entry");
        assert_eq!(
            entries[3]["rel"], "../outside.rs",
            "the hidden entry is kept"
        );

        // Removing everything this build sees leaves what it does not.
        let (_, saved) = edit_with_fallback::<Bookmark>(&root, &[], |list| list.clear());
        saved.unwrap();
        let entries = read(&root)["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["rel"], "../outside.rs");
    }

    /// A local edit settles a remote edit a crashed server left pending
    /// before it replaces the store: the edit's replay is then recognised,
    /// and the bookmark it toggled on stays on.
    #[test]
    fn a_local_edit_settles_a_pending_remote_edit_first() {
        let root = TempDir::new("store-settle");
        let path = store_path::<Bookmark>(&root);
        let toggle = crate::bookmarks::merge_toggle("a.rs", 1, "a".into());
        clew_core::testutil::merge_crashing_after_store_write(&path, &toggle, "w5-1").unwrap_err();
        let (_, saved) = edit_with_fallback::<Bookmark>(&root, &[], |list| {
            crate::bookmarks::toggle(list, "b.rs", 2, "b".into());
        });
        saved.unwrap();
        let replay = clew_core::statefile::merge_file(&path, &toggle, "w5-1").unwrap();
        assert!(!replay.applied, "the replay applied the edit again");
        let rels: Vec<String> = read(&root)
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["rel"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(rels, ["a.rs", "b.rs"]);
    }

    /// A bare array (what every clew writes today) stays a bare array, and an
    /// emptied store removes its file.
    #[test]
    fn a_bare_array_stays_bare_and_an_empty_store_has_no_file() {
        let root = TempDir::new("store-bare");
        let (_, saved) = edit_with_fallback::<Bookmark>(&root, &[], |list| {
            crate::bookmarks::toggle(list, "a.rs", 1, String::new());
        });
        saved.unwrap();
        assert!(read(&root).is_array());
        let (merged, saved) = edit_with_fallback::<Bookmark>(&root, &[], |list| {
            crate::bookmarks::toggle(list, "a.rs", 1, String::new());
        });
        saved.unwrap();
        assert!(merged.is_empty());
        assert!(!store_path::<Bookmark>(&root).exists());
    }
}
