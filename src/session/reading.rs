//! Per-project reading preferences kept in `<root>/.clew/reading.toml`.
//!
//! Currently just the target the `#[cfg]` dimming is evaluated against, stored
//! by its picker label so a team can share "read this as Windows" via the file.

use std::path::{Path, PathBuf};

use crate::inactive::Target;

fn store_path(root: &Path) -> PathBuf {
    root.join(".clew").join("reading.toml")
}

/// Decode a store file's text (shared by disk and protocol paths).
pub fn target_from_text(text: &str) -> Option<Target> {
    let table: toml::Value = toml::from_str(text).ok()?;
    let label = table.get("target")?.as_str()?;
    Some(Target::from_label(label))
}

/// Encode for persistence; `None` (the host default) deletes the file.
pub fn target_to_text(target: &Target) -> Option<String> {
    if target == &Target::host() {
        return None;
    }
    let mut table = toml::Table::new();
    table.insert("target".into(), target.label.clone().into());
    toml::to_string(&table).ok()
}

/// The saved reading target, or `None` when nothing is stored (use the host).
pub fn load_target(root: &Path) -> Option<Target> {
    let text = clew_core::statefile::read_capped(&store_path(root), 64 * 1024)?;
    target_from_text(&text)
}

/// Persist the reading target (or remove the file when it's just the host).
///
/// Last-writer-wins is the semantics here, not an oversight: the store holds a
/// single scalar preference, so two windows on one project cannot accumulate
/// anything for the other to erase — the one that picked a target most recently
/// is the one the reader means. Keyed stores that DO accumulate (bookmarks,
/// notes) merge under a lock instead.
pub fn save_target(root: &Path, target: &Target) -> std::io::Result<()> {
    let path = store_path(root);
    match target_to_text(target) {
        None => clew_core::statefile::remove(&path),
        Some(s) => clew_core::statefile::write_atomic(&path, s.as_bytes()),
    }
}
