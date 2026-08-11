//! Auto-update orchestration: the async tasks that check for, download, and
//! install a new clew release.
//!
//! The pure logic (finding the latest release, streaming the download) lives in
//! `clew_core::update`; the macOS bundle swap lives in `crate::macos::install`.
//! This module wires them to the iced update loop as `Task`s. The App handlers
//! that drive these tasks and hold the state live in `crate::app::updater`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use iced::Task;
use iced::futures::SinkExt;

use crate::Message;
use clew_core::update::{self, Version};

/// The running client's own version (the `clew` package version, which is what
/// releases are tagged with — not clew-core's independent version).
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Ensures the silent startup check runs only once per process, even though
/// every window opens its own `App`.
static STARTUP_CHECKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True the first time it is called in this process, false after — so only the
/// first window's startup fires the auto-check.
pub fn claim_startup_check() -> bool {
    STARTUP_CHECKED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
}

/// The running client version, parsed.
pub fn current_version() -> Version {
    Version::parse(CLIENT_VERSION).unwrap_or(Version {
        major: 0,
        minor: 0,
        patch: 0,
    })
}

/// The GitHub page for a release, used as a manual-install fallback when clew
/// can't swap its own bundle (e.g. a dev build).
pub fn release_page_url(version: &Version) -> String {
    format!("https://github.com/RutaTang/Clew/releases/tag/v{version}")
}

/// Whether clew checks for updates automatically at startup (persisted; default
/// on).
pub fn auto_check_enabled() -> bool {
    clew_core::globalconfig::get("auto_update")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// Persist the auto-check preference, preserving everything else in
/// `config.toml` (see `clew_core::globalconfig`).
pub fn set_auto_check(enabled: bool) -> Result<(), String> {
    clew_core::globalconfig::set_keys(vec![("auto_update".into(), toml::Value::Boolean(enabled))])
}

/// One-shot: query the latest release off the UI thread and report it back.
/// `manual` distinguishes a user-triggered check (which announces "up to date")
/// from the silent startup check.
pub fn check_task(manual: bool) -> Task<Message> {
    Task::perform(
        async move {
            tokio::task::spawn_blocking(update::latest_release)
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
        },
        move |result| Message::UpdateChecked { manual, result },
    )
}

/// What the blocking downloader feeds back to the streaming task.
enum DlPiece {
    Progress(u64, Option<u64>),
    Done(Result<PathBuf, String>),
}

/// Names the private per-attempt directory a download streams into. Also the
/// marker [`discard_download`] checks before deleting a tree.
pub(crate) const DOWNLOAD_PREFIX: &str = "dl";

/// How long a download directory must have sat untouched before
/// [`sweep_stale_downloads`] will remove it. Well beyond any real download, and
/// beyond any plausible stall in one, so a directory another *live* clew is
/// streaming into can never be old enough to qualify.
const STALE_DOWNLOAD_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Most `updates/` entries one sweep will look at, so a directory that somehow
/// filled up cannot turn a launch into a long walk.
const MAX_SWEPT_ENTRIES: usize = 512;

/// A private directory under `parent`, named `<prefix>-<pid>-<n>` and created
/// exclusively. Both halves of an update use it: the download streams into one
/// and the installer stages the verified bundle into another.
///
/// `create_dir`, not `create_dir_all`: an existing entry is a failure we step
/// over rather than a directory we silently adopt, so clew never writes an
/// update into something another process made. The counter is what removes the
/// same-user collision — keyed on the pid alone, two windows updating at once
/// shared one directory, where either one's `remove_dir_all` could delete the
/// other's staged bundle mid-swap.
///
/// Mode 0700 on the leaf, so whatever lands inside is unreachable to other
/// accounts whatever the modes on the data root above it happen to be.
pub fn create_private_dir(parent: &Path, prefix: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let pid = std::process::id();
    for _ in 0..64 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = parent.join(format!("{prefix}-{pid}-{n}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not prepare the update: {e}")),
        }
    }
    Err("could not create a private directory for the update".into())
}

/// Creates and returns where THIS download attempt writes its DMG.
///
/// clew's own data directory, never the system temp dir: the name is
/// predictable, and with `TMPDIR` unset that dir is the shared `/tmp`, where
/// another local user can pre-plant the path as a symlink and have the write
/// land wherever they choose. (The installer verifies the DMG's signature and
/// Team ID before swapping anything in, so this is about where bytes get
/// written, not about what gets installed.)
///
/// With no data directory there is nowhere safe, so this FAILS rather than
/// falling back. The fallback used to be the temp dir — the one location the
/// paragraph above rules out — and the argument does not stop applying because
/// the preferred path is unavailable. `macos::install::staging_parent` refuses
/// on the same grounds, so both halves of an install agree.
///
/// Per ATTEMPT, not per version, and that is the second half of a collision
/// whose staging half `create_private_dir` already fixed: `UpdateState` is
/// per-window while the filesystem is not, so two windows offered the same
/// release both wrote `updates/Clew-<version>.dmg`. `update::download_to`
/// opens with `truncate`, so the second run zeroed the file the first was
/// streaming into, and whichever `UpdateDownloaded` landed first handed
/// `hdiutil` an image the other task was still writing. With a directory per
/// attempt neither run can see, truncate, or install the other's bytes.
///
/// The directory outlives the download, so every route that ends one has to
/// take it: [`discard_download`] runs on a failed download, after an install
/// attempt, on a generation the app has moved past, and on a result that comes
/// back for a window that has since closed. What no runtime route can reach is
/// a clew that stops existing mid-stream, and since this is no longer the temp
/// dir nothing else would ever clear that — hence
/// [`sweep_stale_downloads`] at launch.
fn create_download_dest(version: Version) -> Result<PathBuf, String> {
    Ok(create_private_dir(&updates_dir()?, DOWNLOAD_PREFIX)?.join(format!("Clew-{version}.dmg")))
}

/// Where every download attempt's directory lives. No data directory means no
/// safe place to write one, so this fails rather than falling back (see
/// [`create_download_dest`]).
fn updates_dir() -> Result<PathBuf, String> {
    clew_core::lsp::store::data_root()
        .map(|root| root.join("updates"))
        .ok_or_else(|| "no data directory to download the update into".to_string())
}

/// Drop a download's private directory once its bytes are of no further use.
/// Best effort, and only for a directory [`create_download_dest`] made: this
/// deletes a tree, and the path reaches `install_task` as message data.
pub(crate) fn discard_download(dmg: &Path) {
    let Some(dir) = dmg.parent() else { return };
    let ours = dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_download_dir_name);
    if ours {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// `dl-<pid>-<n>`, exactly as [`create_private_dir`] spells it. Matched in full
/// rather than by the `dl-` prefix alone, because this is the gate in front of a
/// recursive delete and both callers reach it with a path that came from
/// somewhere else — a message, or a directory listing. The two callers share
/// this one predicate on purpose: a sweep that admitted names the discard
/// refused (or the reverse) would be two rules disagreeing about which trees
/// belong to clew.
fn is_download_dir_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(DOWNLOAD_PREFIX)
        .and_then(|r| r.strip_prefix('-'))
    else {
        return false;
    };
    let mut parts = rest.split('-');
    let (Some(pid), Some(n), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(pid) && digits(n)
}

/// Remove download directories left behind by clew runs that no longer exist.
///
/// [`discard_download`] covers every route that *ends* a download: a failure, an
/// install attempt, a superseded generation, a window that vanished under a
/// finished stream. What it cannot cover is a clew that stops existing while one
/// is still streaming — a quit, a crash, a log-out. Since the destination moved
/// off the temp dir nothing else clears those any more: not the next launch, not
/// a later successful update, and no longer the reboot that used to empty
/// `/tmp`. A whole release image would sit under the data root for good. This is
/// the missing sweep, run once per launch.
///
/// Deliberately narrow, because it deletes trees under a directory other
/// processes are using at the same time. Only entries directly under
/// `<data root>/updates`; only real directories (not a symlink planted under one
/// of these names); only names of the exact `dl-<pid>-<n>` shape; and only ones
/// nothing has touched for [`STALE_DOWNLOAD_AGE`]. That last check is what keeps
/// a download another *live* clew is streaming into safe, including one whose
/// pid has since been reused — a stalled but live download just keeps its
/// directory, which is the safe way to be wrong.
///
/// `stage-<pid>-<n>` is deliberately NOT swept, however old it looks. The swap
/// helper waits for clew's pid to die and only *then* works out of the staging
/// directory, so "the owner is gone" is precisely the moment its contents are
/// about to be installed; deleting it would pull the bundle out mid-swap. The
/// helper prunes its own directory, and the case where the helper never finishes
/// remains the accepted residual documented at `macos::install::install_dmg`.
///
/// Blocking (a `read_dir` plus, rarely, removing a disk image); call it off the
/// UI thread.
pub fn sweep_stale_downloads() {
    let Ok(updates) = updates_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&updates) else {
        return;
    };
    for entry in entries.take(MAX_SWEPT_ENTRIES).flatten() {
        let name = entry.file_name();
        if !name.to_str().is_some_and(is_download_dir_name) {
            continue;
        }
        // `file_type` does not follow symlinks, so a symlink named `dl-1-0` is
        // not a directory here and is left where it is.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();
        if last_touched(&path).is_none_or(|age| age < STALE_DOWNLOAD_AGE) {
            continue;
        }
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// How long ago anything in `dir` — the directory itself or a file directly
/// inside it — was last written. `None` when that cannot be established (an
/// unreadable directory, or an mtime in the future), which the caller reads as
/// "leave it alone".
///
/// The entries have to be looked at, not just the directory: a directory's own
/// mtime moves when a name is created or removed and then stops, so an active
/// multi-minute download into a file created at the start would look untouched
/// the whole way through.
fn last_touched(dir: &Path) -> Option<Duration> {
    let mut newest = std::fs::metadata(dir).ok()?.modified().ok()?;
    for entry in std::fs::read_dir(dir)
        .ok()?
        .take(MAX_SWEPT_ENTRIES)
        .flatten()
    {
        if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
            newest = newest.max(modified);
        }
    }
    SystemTime::now().duration_since(newest).ok()
}

/// Streamed download of the DMG at `url`, emitting throttled progress messages
/// and a final `UpdateDownloaded`. `generation` lets the handler drop a
/// superseded run's late messages.
pub fn download_task(url: String, version: Version, generation: u64) -> Task<Message> {
    let stream = iced::stream::channel(
        256,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            // Claimed when the download actually starts rather than when the
            // task is built: the destination is a directory now, and a task
            // dropped before it ever ran would leave one behind.
            let dest = match create_download_dest(version) {
                Ok(d) => d,
                Err(e) => {
                    let _ = output
                        .send(Message::UpdateDownloaded {
                            generation,
                            result: Err(e),
                        })
                        .await;
                    return;
                }
            };
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DlPiece>();
            let dl_dest = dest.clone();
            // Blocking producer: download in chunks, forwarding throttled progress.
            tokio::task::spawn_blocking(move || {
                let mut last = Instant::now();
                let res = update::download_to(&url, &dl_dest, |done, total| {
                    if last.elapsed() >= Duration::from_millis(100) {
                        last = Instant::now();
                        let _ = tx.send(DlPiece::Progress(done, total));
                    }
                });
                // A failed download leaves a partial image nobody will install
                // — and this attempt's directory is nobody else's, so taking it
                // with us costs no other run its bytes.
                if res.is_err() {
                    discard_download(&dl_dest);
                }
                let _ = tx.send(DlPiece::Done(res.map(|()| dl_dest.clone())));
            });
            // Drain the channel into UI messages.
            while let Some(piece) = rx.recv().await {
                let (msg, done) = match piece {
                    DlPiece::Progress(done, total) => (
                        Message::UpdateDownloadProgress {
                            generation,
                            done,
                            total,
                        },
                        false,
                    ),
                    DlPiece::Done(result) => {
                        (Message::UpdateDownloaded { generation, result }, true)
                    }
                };
                if output.send(msg).await.is_err() || done {
                    break;
                }
            }
        },
    );
    Task::run(stream, |m| m)
}

/// Verify the downloaded DMG, swap the bundle in, and launch the relauncher.
/// On success the app should quit so the detached helper can finish. `reopen` is
/// the project to reopen after relaunch, if any. The download's own directory
/// goes on either outcome ([`discard_download`]) — a task that panics outright
/// is the one case that leaves it.
pub fn install_task(dmg: PathBuf, reopen: Option<PathBuf>) -> Task<Message> {
    Task::perform(
        async move {
            #[cfg(target_os = "macos")]
            {
                tokio::task::spawn_blocking(move || {
                    let done = crate::macos::install::install_dmg(&dmg, reopen);
                    // The image has done its job on either outcome: a success
                    // already copied the verified app out of it and detached
                    // the volume, and a retry after a failure downloads afresh
                    // rather than reusing this file.
                    discard_download(&dmg);
                    done
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = reopen;
                discard_download(&dmg);
                Err("self-install is only supported on macOS".to_string())
            }
        },
        Message::UpdateInstalled,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Every update directory — a download's and an install's alike — is its
    /// own, private to this user, and an entry that already exists is stepped
    /// over rather than adopted: neither a second window nor a pre-planted
    /// directory can be the one clew writes an update into.
    #[test]
    fn each_update_directory_is_fresh_private_and_never_adopted() {
        let parent = std::env::temp_dir().join(format!("clew-privdir-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);

        let first = create_private_dir(&parent, "stage").unwrap();
        assert!(first.starts_with(&parent));
        assert_eq!(
            std::fs::metadata(&first).unwrap().permissions().mode() & 0o777,
            0o700,
            "what lands inside must not be reachable from another account"
        );

        // Pre-plant the next name in the sequence, with something in it, the
        // way a second process (or an attacker on a shared path) would.
        let name = first.file_name().unwrap().to_str().unwrap();
        let n: u64 = name.rsplit('-').next().unwrap().parse().unwrap();
        let planted = parent.join(format!("stage-{}-{}", std::process::id(), n + 1));
        std::fs::create_dir(&planted).unwrap();
        std::fs::write(planted.join("marker"), "x").unwrap();

        let second = create_private_dir(&parent, "stage").unwrap();
        assert_ne!(second, first, "two attempts must not share a directory");
        assert_ne!(second, planted, "an existing directory must not be adopted");
        assert_eq!(
            std::fs::read_dir(&second).unwrap().count(),
            0,
            "a fresh directory starts empty"
        );

        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Two windows offered the same release both download it, and the second
    /// used to open the first's half-written `Clew-<version>.dmg` with
    /// `truncate` — zeroing bytes the first had already streamed, under an
    /// installer that could pick the file up at any moment. Each attempt must
    /// therefore get a destination the other cannot name, let alone read.
    #[test]
    fn two_download_attempts_never_share_a_file() {
        let _env = clew_core::env_lock();
        let data = std::env::var_os("CLEW_DATA_DIR");
        let root = std::env::temp_dir().join(format!("clew-dl-race-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // SAFETY: env mutation serialized by env_lock.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &root) };

        let v = Version {
            major: 9,
            minor: 9,
            patch: 9,
        };
        let window_a = create_download_dest(v).expect("a data root gives a destination");
        let window_b = create_download_dest(v).expect("a data root gives a destination");
        assert_ne!(window_a, window_b, "two attempts must not share a file");
        assert_ne!(
            window_a.parent(),
            window_b.parent(),
            "…and not a directory either, or one could still be swept away"
        );

        // A partially written image in one attempt is invisible to the other:
        // nothing at B's path, so nothing there for an install to read and
        // nothing for B's own `truncate` open to cut into.
        std::fs::write(&window_a, b"half an image").unwrap();
        assert!(
            !window_b.exists(),
            "one attempt's partial download must not be reachable as the other's"
        );

        // SAFETY: env mutation serialized by env_lock.
        unsafe {
            match data {
                Some(d) => std::env::set_var("CLEW_DATA_DIR", d),
                None => std::env::remove_var("CLEW_DATA_DIR"),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The download's directory is this attempt's alone, so it goes when its
    /// bytes stop being useful — but `install_task` is handed a path that came
    /// through a message, and this deletes a TREE. Only a directory the
    /// download path itself named may be removed.
    #[test]
    fn only_our_own_download_directory_is_ever_deleted() {
        let root =
            std::env::temp_dir().join(format!("clew-dl-discard-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ours = create_private_dir(&root, DOWNLOAD_PREFIX).unwrap();
        let dmg = ours.join("Clew-9.9.9.dmg");
        std::fs::write(&dmg, b"image").unwrap();
        discard_download(&dmg);
        assert!(!ours.exists(), "the attempt's own directory is removed");

        // Anything else keeps its directory, whatever the file is called.
        let elsewhere = root.join("Documents");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let stray = elsewhere.join("Clew-9.9.9.dmg");
        std::fs::write(&stray, b"image").unwrap();
        discard_download(&stray);
        assert!(
            elsewhere.exists() && stray.exists(),
            "a directory this module did not create must be left alone"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Push one path's mtime `by` into the past. Only that path — the sweep has
    /// to be shown looking past a directory's own timestamp.
    fn age(path: &Path, by: Duration) {
        let when = SystemTime::now() - by;
        let f = std::fs::File::open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    /// A clew that dies mid-download (quit, crash, log-out) never reaches
    /// `discard_download`, and since the destination moved off the temp dir
    /// nothing else ever clears it — not the next launch, not a later update,
    /// not a reboot. A whole release image stayed under the data root for good.
    /// The sweep that fixes that runs over a directory other clews are using at
    /// the same time, so what it must NOT take is the larger half of the test:
    /// a download still being written, a staging directory (whose owner being
    /// gone is exactly when its bundle is about to be installed), and anything
    /// that is not one of clew's own `dl-<pid>-<n>` names.
    #[test]
    fn only_a_download_no_live_clew_is_writing_is_swept() {
        let _env = clew_core::env_lock();
        let data = std::env::var_os("CLEW_DATA_DIR");
        let root = std::env::temp_dir().join(format!("clew-dl-sweep-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // SAFETY: env mutation serialized by env_lock.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &root) };
        let updates = updates_dir().unwrap();

        // Stranded by a run that is gone: old directory, old image.
        let stale = create_private_dir(&updates, DOWNLOAD_PREFIX).unwrap();
        let stale_dmg = stale.join("Clew-9.9.9.dmg");
        std::fs::write(&stale_dmg, b"image").unwrap();
        age(&stale_dmg, STALE_DOWNLOAD_AGE * 2);
        age(&stale, STALE_DOWNLOAD_AGE * 2);

        // In flight right now. A directory's own mtime stops moving once the
        // file inside it exists, so this one looks just as old as the stale one
        // until the file is looked at.
        let live = create_private_dir(&updates, DOWNLOAD_PREFIX).unwrap();
        std::fs::write(live.join("Clew-9.9.9.dmg"), b"image").unwrap();
        age(&live, STALE_DOWNLOAD_AGE * 2);

        // A staging directory, and two things this module did not name.
        let staging = create_private_dir(&updates, "stage").unwrap();
        std::fs::write(staging.join("marker"), b"x").unwrap();
        age(&staging, STALE_DOWNLOAD_AGE * 2);
        let odd = updates.join("dl-notapid");
        std::fs::create_dir(&odd).unwrap();
        age(&odd, STALE_DOWNLOAD_AGE * 2);
        let loose = updates.join("Clew-1.0.0.dmg");
        std::fs::write(&loose, b"image").unwrap();
        age(&loose, STALE_DOWNLOAD_AGE * 2);

        sweep_stale_downloads();

        assert!(!stale.exists(), "an abandoned download must not survive");
        assert!(
            live.exists(),
            "a download another clew is still writing must be left alone"
        );
        assert!(
            staging.exists(),
            "the swap helper works out of the staging directory after its clew is gone"
        );
        assert!(odd.exists() && loose.exists(), "only clew's own names go");

        // SAFETY: env mutation serialized by env_lock.
        unsafe {
            match data {
                Some(d) => std::env::set_var("CLEW_DATA_DIR", d),
                None => std::env::remove_var("CLEW_DATA_DIR"),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The one place a downloaded update may be written is clew's own data
    /// directory. Falling back to the shared temp dir when there is none was
    /// the exact weakness the destination was moved off /tmp to avoid: a
    /// predictable name another local user can pre-plant as a symlink.
    #[test]
    fn a_download_without_a_data_directory_fails_instead_of_using_temp() {
        let _env = clew_core::env_lock();
        let home = std::env::var_os("HOME");
        let data = std::env::var_os("CLEW_DATA_DIR");
        let root = std::env::temp_dir().join("clew-download-dest-test");
        // SAFETY: env mutation serialized by env_lock.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &root) };
        let ok = create_download_dest(Version {
            major: 9,
            minor: 9,
            patch: 9,
        })
        .expect("a data root gives a destination");
        assert!(
            ok.starts_with(&root) && ok.ends_with("Clew-9.9.9.dmg"),
            "written under the data root, got {ok:?}"
        );

        // No data directory at all: nowhere safe, so nowhere.
        // SAFETY: env mutation serialized by env_lock.
        unsafe {
            std::env::remove_var("CLEW_DATA_DIR");
            std::env::remove_var("HOME");
        }
        let err = create_download_dest(Version {
            major: 9,
            minor: 9,
            patch: 9,
        })
        .expect_err("no data directory must fail the download");
        assert!(err.contains("no data directory"), "{err}");

        // SAFETY: env mutation serialized by env_lock.
        unsafe {
            match home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
            match data {
                Some(d) => std::env::set_var("CLEW_DATA_DIR", d),
                None => std::env::remove_var("CLEW_DATA_DIR"),
            }
        }
    }
}
