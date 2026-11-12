//! The folders opened on this machine, newest first, kept between runs of
//! clew: the welcome screen offers them again. With nothing of the kind,
//! a reader came back to every project through the folder dialog, though
//! clew reopens a project where it was left.
//!
//! Only folders on this machine: a remote host's are its own, reached by
//! connecting to it. Kept in clew's own data directory (`recent.json`),
//! never in a project; a file this build cannot read is left as it is and
//! read as no folders. Each function takes the file's path (the app's
//! `recent_store`), so a test keeps its own and none is shared.

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

/// Where the folders are kept: `recent.json` in clew's data directory.
pub(crate) fn store() -> Option<PathBuf> {
    Some(clew_core::lsp::store::data_root()?.join(FILE))
}

/// The folders opened here before, newest first — those there now. Off
/// the UI thread: each is looked at on disk, and one on a network share no
/// longer reachable waits for its timeout. One deleted is taken out of the
/// file, or it kept a place the welcome screen could offer; one out of
/// reach only for now — on a drive not plugged in, a share offline, a
/// folder it may not read — is kept for when it is back ([`deleted`]).
pub(crate) fn load_from(store: &Path) -> Vec<PathBuf> {
    let Some(text) = clew_core::statefile::read_capped(store, MAX_BYTES) else {
        return Vec::new();
    };
    let Ok(file) = serde_json::from_str::<RecentFile>(&text) else {
        return Vec::new();
    };
    let (live, away): (Vec<PathBuf>, Vec<PathBuf>) = file
        .folders
        .into_iter()
        .partition(|p| p.is_absolute() && p.is_dir());
    let gone: Vec<PathBuf> = away.into_iter().filter(|p| deleted(p)).collect();
    if !gone.is_empty() {
        edit(store, |folders| folders.retain(|p| !gone.contains(p)));
    }
    live.into_iter().take(KEPT).collect()
}

/// Whether `folder` is gone for good: not there, though the folder it was
/// in is — not on a drive unplugged or a share offline, whose folders all
/// went with it, nor one that could not be looked at (no leave to read).
/// Each was forgotten as Clew started without it, and stayed so once back.
fn deleted(folder: &Path) -> bool {
    !folder.is_absolute()
        || (matches!(folder.try_exists(), Ok(false))
            && folder.parent().is_some_and(|parent| parent.is_dir()))
}

impl crate::App {
    /// Read the folders opened here before, for the welcome screen.
    pub(crate) fn read_recent_folders(&self) -> iced::Task<crate::Message> {
        let Some(store) = self.recent_store.clone() else {
            return iced::Task::none();
        };
        iced::Task::perform(
            async move {
                tokio::task::spawn_blocking(move || load_from(&store))
                    .await
                    .unwrap_or_default()
            },
            |read| crate::Message::Project(crate::ProjectMsg::RecentRead(read)),
        )
    }
}

/// `root` opened: first among the recent folders, once. Best effort — a
/// folder not remembered costs a dialog, never the open.
pub(crate) fn remember_in(store: &Path, root: &Path) {
    edit(store, |folders| {
        folders.retain(|p| p != root);
        folders.insert(0, root.to_path_buf());
        folders.truncate(KEPT);
    });
}

/// `root` asked for and not there: no longer offered.
pub(crate) fn forget_in(store: &Path, root: &Path) {
    edit(store, |folders| folders.retain(|p| p != root));
}

/// Read, change and write the list, under the file's lock: two windows,
/// or two clews, opening folders at once each keep theirs. Written over
/// only where it reads as this build's, or is not there.
fn edit(store: &Path, change: impl FnOnce(&mut Vec<PathBuf>)) {
    // The data directory holds the API keys' config too: made private, as
    // everything else that makes it does.
    if let Some(dir) = store.parent()
        && clew_core::derived::ensure_private_dir(dir).is_err()
    {
        return;
    }
    let Ok(_lock) = clew_core::statefile::lock(store) else {
        return;
    };
    let mut folders = match clew_core::statefile::read_capped(store, MAX_BYTES) {
        Some(text) => match serde_json::from_str::<RecentFile>(&text) {
            Ok(f) => f.folders,
            Err(_) => return,
        },
        None if store.exists() => return,
        None => Vec::new(),
    };
    let before = folders.clone();
    change(&mut folders);
    if folders == before {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&RecentFile { folders }) {
        let _ = clew_core::statefile::write_atomic(store, &bytes);
    }
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

    /// Opened folders come back newest first, each once, as many as kept;
    /// one no longer there is not offered, and leaves the file; one asked
    /// for and not found is forgotten.
    #[test]
    fn opened_folders_come_back_newest_first() {
        let data = clew_core::testutil::TempDir::new("recent-data");
        let store = data.path().join("state").join(FILE);
        let dirs: Vec<PathBuf> = (0..7)
            .map(|i| {
                let d = data.path().join(format!("p{i}"));
                std::fs::create_dir_all(&d).unwrap();
                d
            })
            .collect();
        assert!(load_from(&store).is_empty());
        for d in &dirs[..3] {
            remember_in(&store, d);
        }
        remember_in(&store, &dirs[0]);
        assert_eq!(
            load_from(&store),
            vec![dirs[0].clone(), dirs[2].clone(), dirs[1].clone()]
        );
        for d in &dirs[3..] {
            remember_in(&store, d);
        }
        let kept = load_from(&store);
        assert_eq!(kept.len(), KEPT);
        assert_eq!(kept[0], dirs[6]);
        // One out of reach for now — its whole drive away — is not
        // offered, and is kept for when it is back.
        let away = data.path().join("drive");
        let on_drive = away.join("project");
        std::fs::create_dir_all(&on_drive).unwrap();
        remember_in(&store, &on_drive);
        let unplugged = data.path().join("drive-unplugged");
        std::fs::rename(&away, &unplugged).unwrap();
        assert!(!load_from(&store).contains(&on_drive), "offered while away");
        std::fs::rename(&unplugged, &away).unwrap();
        assert_eq!(load_from(&store)[0], on_drive, "forgotten while away");
        forget_in(&store, &on_drive);
        std::fs::remove_dir(&dirs[6]).unwrap();
        assert_eq!(
            load_from(&store)[0],
            dirs[5],
            "a folder gone is not offered"
        );
        let text = std::fs::read_to_string(&store).unwrap();
        assert!(
            !text.contains("p6"),
            "a folder gone stays in the file: {text}"
        );
        forget_in(&store, &dirs[5]);
        assert_eq!(load_from(&store)[0], dirs[4], "forgotten");
        // The directory it makes is private: it holds the keys' config.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
        // A file it cannot read is left as it is.
        std::fs::write(&store, "not json").unwrap();
        remember_in(&store, &dirs[1]);
        assert_eq!(std::fs::read_to_string(&store).unwrap(), "not json");
        assert!(load_from(&store).is_empty());
    }
}
