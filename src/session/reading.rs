//! Per-project reading preferences kept in `<root>/.clew/reading.toml`.
//!
//! Currently just the target the `#[cfg]` dimming is evaluated against, stored
//! by its picker label so a team can share "read this as Windows" via the file.

use std::path::{Path, PathBuf};

use crate::inactive::Target;

fn store_path(root: &Path) -> PathBuf {
    root.join(".clew").join("reading.toml")
}

/// The saved reading target, or `None` when nothing is stored (use the host).
pub fn load_target(root: &Path) -> Option<Target> {
    let text = clew_core::statefile::read_capped(&store_path(root), 64 * 1024)?;
    let table: toml::Value = toml::from_str(&text).ok()?;
    let label = table.get("target")?.as_str()?;
    Some(Target::from_label(label))
}

/// Persist the reading target (or remove the file when it's just the host).
pub fn save_target(root: &Path, target: &Target) -> std::io::Result<()> {
    let path = store_path(root);
    if target == &Target::host() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let mut table = toml::Table::new();
    table.insert("target".into(), target.label.clone().into());
    let s = toml::to_string(&table).map_err(|e| std::io::Error::other(e.to_string()))?;
    clew_core::statefile::write_atomic(&path, s.as_bytes())
}
