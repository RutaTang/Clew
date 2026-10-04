//! Per-project reading preferences kept in `<root>/.clew/reading.toml`.
//!
//! Currently just the target the `#[cfg]` dimming is evaluated against, stored
//! by its picker label. It is one reader's view setting, so the `.gitignore`
//! the state layer writes into `.clew/` keeps it out of the repository (a team
//! that does want to share "read this as Windows" can still commit it on
//! purpose).
//!
//! Like every store here, a file that exists but cannot be read or parsed is
//! reported by [`load_target_checked`] and never overwritten by
//! [`save_target`] — and a save changes only the `target` key: anything else
//! in the file (a hand edit, a newer clew's setting) is written back as it
//! was.

use std::path::{Path, PathBuf};

use clew_core::statefile::StoreError;

use crate::inactive::Target;

/// The file holds one short setting; anything larger is not one clew wrote.
const MAX_READING_BYTES: u64 = clew_core::statefile::MAX_TOML_STATE_BYTES;

fn store_path(root: &Path) -> PathBuf {
    root.join(".clew").join("reading.toml")
}

/// Decode a store file's text (shared by disk and protocol paths): `Ok(None)`
/// when it names no target, an error when it is not TOML at all.
pub fn try_target_from_text(text: &str) -> Result<Option<Target>, StoreError> {
    let table: toml::Table =
        toml::from_str(text).map_err(|e| StoreError::Unparseable(e.to_string()))?;
    Ok(table
        .get("target")
        .and_then(|v| v.as_str())
        .map(Target::from_label))
}

/// The remote equivalent of `save_target`: a change to this key alone,
/// resolved against the current file under the server's state lock.
pub fn merge_target(target: &Target) -> clew_protocol::StateMerge {
    clew_protocol::StateMerge {
        key_fields: vec!["target".into()],
        key: Vec::new(),
        edit: clew_protocol::StateEdit::TomlString(
            (target != &Target::host()).then(|| target.label.clone()),
        ),
        delete_when_empty: true,
    }
}

/// The saved reading target: `Ok(None)` when nothing is stored (use the
/// host), an error when the file exists but cannot be read or parsed.
pub fn load_target_checked(root: &Path) -> Result<Option<Target>, StoreError> {
    match clew_core::statefile::read_capped_checked(&store_path(root), MAX_READING_BYTES) {
        Ok(None) => Ok(None),
        Ok(Some(text)) => try_target_from_text(&text),
        Err(e) => Err(StoreError::Refused(e)),
    }
}

/// [`load_target_checked`] for DISPLAY: an unreadable file reads as "no
/// target".
#[cfg(test)]
pub fn load_target(root: &Path) -> Option<Target> {
    load_target_checked(root).ok().flatten()
}

/// Persist the reading target: set the file's `target` key, or remove it for
/// the host default — every other key stays as the file has it, and the file
/// itself goes only when nothing is left in it.
///
/// Last-writer-wins is the semantics for the target, not an oversight: it is
/// a single scalar preference, so two windows on one project cannot
/// accumulate anything for the other to erase — the one that picked a target
/// most recently is the one the reader means. The read-modify-write still
/// runs under the store lock, so the OTHER keys cannot be lost to a
/// concurrent save either.
///
/// A file that exists but cannot be read or parsed is left alone (the save
/// fails with the reason): it may hold settings a hand edit or a newer clew
/// added.
pub fn save_target(root: &Path, target: &Target) -> std::io::Result<()> {
    let path = store_path(root);
    let _exclusive = clew_core::statefile::lock(&path)?;
    let current = match clew_core::statefile::read_capped_checked(&path, MAX_READING_BYTES) {
        Ok(text) => text,
        Err(e) => return Err(StoreError::Refused(e).into()),
    };
    let value = (target != &Target::host()).then_some(target.label.as_str());
    let merged =
        clew_core::statefile::merge_toml_string_checked(current.as_deref(), "target", value, true)?;
    // A remote edit's reply may have died after its write. Settle its
    // ledger before the local writer moves that version of the file.
    clew_core::statefile::settle_pending_edit(&path)?;
    match merged {
        None => clew_core::statefile::remove(&path),
        Some(text) => clew_core::statefile::write_atomic(&path, text.as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(name: &str) -> clew_core::testutil::TempDir {
        clew_core::testutil::TempDir::new(name)
    }

    #[test]
    fn a_target_round_trips_and_the_host_removes_the_file() {
        let root = fresh("clew-reading-roundtrip");
        let windows = Target::from_label("Windows (x86_64)");
        assert_ne!(windows, Target::host(), "a real preset, not the fallback");
        save_target(&root, &windows).unwrap();
        assert_eq!(load_target_checked(&root).unwrap(), Some(windows));
        save_target(&root, &Target::host()).unwrap();
        assert!(!store_path(&root).exists());
        assert_eq!(load_target_checked(&root).unwrap(), None);
    }

    /// A reading.toml clew cannot parse is reported, and not replaced by the
    /// next pick in the target menu.
    #[test]
    fn an_unparseable_reading_file_is_never_overwritten() {
        let root = fresh("clew-reading-unparseable");
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        let broken = "target = \"Windows\"\n[unterminated\n";
        std::fs::write(store_path(&root), broken).unwrap();
        assert!(load_target_checked(&root).is_err());
        assert_eq!(load_target(&root), None);
        assert!(save_target(&root, &Target::from_label("Linux (x86_64)")).is_err());
        assert!(save_target(&root, &Target::host()).is_err(), "nor deleted");
        assert_eq!(std::fs::read_to_string(store_path(&root)).unwrap(), broken);
    }

    /// A save changes the target and nothing else: keys this build does not
    /// know (a hand edit, a newer clew) survive picking a target and going
    /// back to the host default, and so does the file holding them.
    #[test]
    fn a_save_keeps_the_keys_it_does_not_own() {
        let root = fresh("reading-other-keys");
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        std::fs::write(store_path(&root), "wrap = true\n\n[future]\ncolumns = 3\n").unwrap();
        let windows = Target::from_label("Windows (x86_64)");
        save_target(&root, &windows).unwrap();
        let table: toml::Table =
            toml::from_str(&std::fs::read_to_string(store_path(&root)).unwrap()).unwrap();
        assert_eq!(table["wrap"].as_bool(), Some(true));
        assert_eq!(table["future"]["columns"].as_integer(), Some(3));
        assert_eq!(load_target_checked(&root).unwrap(), Some(windows));

        save_target(&root, &Target::host()).unwrap();
        let table: toml::Table =
            toml::from_str(&std::fs::read_to_string(store_path(&root)).unwrap()).unwrap();
        assert!(!table.contains_key("target"));
        assert_eq!(table["wrap"].as_bool(), Some(true));
        assert_eq!(load_target_checked(&root).unwrap(), None);
    }

    #[test]
    fn local_and_remote_target_writers_preserve_each_others_current_keys() {
        let root = fresh("reading-local-remote");
        let path = store_path(&root);
        clew_core::statefile::write_atomic(&path, b"wrap = true\n").unwrap();
        let windows = Target::from_label("Windows (x86_64)");
        let remote = merge_target(&windows);
        clew_core::statefile::merge_file(&path, &remote, "remote-1").unwrap();
        save_target(&root, &Target::from_label("Linux (x86_64)")).unwrap();
        let before = clew_core::statefile::read_checked(&path).unwrap();
        let replay = clew_core::statefile::merge_file(&path, &remote, "remote-1").unwrap();
        assert!(!replay.applied);
        assert_eq!(replay.text, before);
        save_target(&root, &Target::host()).unwrap();
        assert_eq!(load_target_checked(&root).unwrap(), None);
        let table: toml::Table = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(table["wrap"].as_bool(), Some(true));
    }

    #[test]
    fn a_local_target_save_refuses_newer_schema_and_oversized_preferences() {
        let root = fresh("reading-local-refusals");
        let path = store_path(&root);
        for text in [
            "schema_version = 2\ntarget = \"Windows (x86_64)\"\n".to_string(),
            format!("#{}\n", "x".repeat(MAX_READING_BYTES as usize)),
        ] {
            clew_core::statefile::write_atomic(&path, text.as_bytes()).unwrap();
            assert!(save_target(&root, &Target::from_label("Linux (x86_64)")).is_err());
            assert!(save_target(&root, &Target::host()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
    }
}
