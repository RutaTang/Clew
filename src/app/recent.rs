//! The folders opened on this machine, newest first, kept between runs of
//! clew: the welcome screen offers them again. With nothing of the kind,
//! a reader came back to every project through the folder dialog, though
//! clew reopens a project where it was left.
//!
//! Only folders on this machine: a remote host's are its own, reached by
//! connecting to it. Kept in clew's own data directory (`recent.json`),
//! never in a project; a file this build cannot read is left as it is and
//! read as no folders.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file, in clew's data directory.
const FILE: &str = "recent.json";
/// How many are kept: as many as the welcome screen offers.
pub(crate) const KEPT: usize = 5;
/// The most of the file read: a few paths.
const MAX_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize, Default)]
struct RecentFile {
    folders: Vec<PathBuf>,
}

fn path() -> Option<PathBuf> {
    Some(clew_core::lsp::store::data_root()?.join(FILE))
}

/// The folders opened here before, newest first — those still there.
pub(crate) fn load() -> Vec<PathBuf> {
    let Some(path) = path() else {
        return Vec::new();
    };
    let Some(text) = clew_core::statefile::read_capped(&path, MAX_BYTES) else {
        return Vec::new();
    };
    serde_json::from_str::<RecentFile>(&text)
        .map(|f| f.folders)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.is_absolute() && p.is_dir())
        .take(KEPT)
        .collect()
}

/// `root` opened: first among the recent folders, once. Best effort — a
/// folder not remembered costs a dialog, never the open.
pub(crate) fn remember(root: &Path) {
    let Some(path) = path() else {
        return;
    };
    // Written over only where it reads as this build's, or is not there.
    let existing = match clew_core::statefile::read_capped(&path, MAX_BYTES) {
        Some(text) => match serde_json::from_str::<RecentFile>(&text) {
            Ok(f) => f.folders,
            Err(_) => return,
        },
        None if path.exists() => return,
        None => Vec::new(),
    };
    let mut folders = vec![root.to_path_buf()];
    folders.extend(existing.into_iter().filter(|p| p != root).take(KEPT - 1));
    let Ok(bytes) = serde_json::to_vec_pretty(&RecentFile { folders }) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = clew_core::statefile::write_atomic(&path, &bytes);
}

/// What a recent folder is called on the welcome screen: its name, and
/// where it is, home written `~`.
pub(crate) fn said(folder: &Path) -> (String, String) {
    let name = folder.file_name().map_or_else(
        || folder.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let parent = folder.parent().map(Path::to_path_buf).unwrap_or_default();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let place = match home.as_deref().and_then(|h| parent.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => parent.display().to_string(),
    };
    (name, place)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opened folders come back newest first, each once, as many as kept,
    /// and one no longer there is not offered.
    #[test]
    fn opened_folders_come_back_newest_first() {
        let data = clew_core::testutil::TempDir::new("recent-data");
        let _data = crate::app::tests::data_dir_override(data.path());
        let dirs: Vec<PathBuf> = (0..7)
            .map(|i| {
                let d = data.path().join(format!("p{i}"));
                std::fs::create_dir_all(&d).unwrap();
                d
            })
            .collect();
        assert!(load().is_empty());
        for d in &dirs[..3] {
            remember(d);
        }
        remember(&dirs[0]);
        assert_eq!(
            load(),
            vec![dirs[0].clone(), dirs[2].clone(), dirs[1].clone()]
        );
        for d in &dirs[3..] {
            remember(d);
        }
        let kept = load();
        assert_eq!(kept.len(), KEPT);
        assert_eq!(kept[0], dirs[6]);
        std::fs::remove_dir(&dirs[6]).unwrap();
        assert_eq!(load()[0], dirs[5], "a folder gone is not offered");
        // A file it cannot read is left as it is.
        std::fs::write(data.path().join(FILE), "not json").unwrap();
        remember(&dirs[1]);
        assert_eq!(
            std::fs::read_to_string(data.path().join(FILE)).unwrap(),
            "not json"
        );
        assert!(load().is_empty());
    }
}
