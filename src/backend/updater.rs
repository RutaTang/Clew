//! Auto-update orchestration: the async tasks that check for, download, and
//! install a new clew release.
//!
//! The pure logic (finding the latest release, streaming the download) lives in
//! `clew_core::update`; the macOS bundle swap lives in `crate::macos::install`.
//! This module wires them to the iced update loop as `Task`s. The App handlers
//! that drive these tasks and hold the state live in `crate::app::updater`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime};

use iced::Task;
use iced::futures::SinkExt;

use crate::Message;
use crate::UpdaterMsg;
use clew_core::update::{self, Version};

/// The version this binary was built as (the `clew` package version — not
/// clew-core's independent one). What [`current_version`] falls back to
/// outside an app bundle; the release workflow asserts that a release's tag,
/// this version and the bundle's `CFBundleShortVersionString` all agree.
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

/// The running client's version: the `CFBundleShortVersionString` of the app
/// bundle clew runs from, else — outside a bundle (a `cargo run` build, the
/// tests) — [`CLIENT_VERSION`]. Read once per process.
///
/// The bundle's version is the one that matters for updates: the release
/// stamps it from the tag, and it is the very field an update's own bundle is
/// judged by (`macos::install::check_candidate_version`, whose no-downgrade
/// rule compares against this). Using the crate version here, as this did,
/// made that comparison silently depend on the tag and `Cargo.toml` agreeing.
pub fn current_version() -> Version {
    static RUNNING: std::sync::OnceLock<Version> = std::sync::OnceLock::new();
    *RUNNING.get_or_init(|| running_version(bundle_short_version().as_deref(), CLIENT_VERSION))
}

/// [`current_version`]'s rule: the bundle's version when it declares a usable
/// one, else the crate's (else 0.0.0, which no release is older than).
fn running_version(bundle: Option<&str>, crate_version: &str) -> Version {
    let built = Version::parse(crate_version);
    if let Some(declared) = bundle {
        match Version::parse(declared) {
            Some(version) => {
                if built != Some(version) {
                    eprintln!(
                        "[clew] this app bundle is version {declared} but was built as \
                         {crate_version}; updates are compared against {declared}"
                    );
                }
                return version;
            }
            None => eprintln!(
                "[clew] this app bundle's version {declared:?} is not a version; using \
                 {crate_version}"
            ),
        }
    }
    built.unwrap_or(Version {
        major: 0,
        minor: 0,
        patch: 0,
    })
}

/// The `CFBundleShortVersionString` of the app bundle clew runs from, or
/// `None` when it does not run from one.
#[cfg(target_os = "macos")]
fn bundle_short_version() -> Option<String> {
    use objc2_foundation::{NSBundle, NSString, ns_string};
    // A bare binary's "main bundle" is its directory; only an installed .app
    // has a version to read.
    crate::macos::install::installed_bundle()?;
    let value = NSBundle::mainBundle()
        .objectForInfoDictionaryKey(ns_string!("CFBundleShortVersionString"))?;
    Some(value.downcast::<NSString>().ok()?.to_string())
}

#[cfg(not(target_os = "macos"))]
fn bundle_short_version() -> Option<String> {
    None
}

/// The GitHub page for a release, used as a manual-install fallback when clew
/// can't swap its own bundle (e.g. a dev build).
pub fn release_page_url(version: &Version) -> String {
    format!("https://github.com/RutaTang/Clew/releases/tag/v{version}")
}

/// The persisted answer to "check for updates automatically?": `None` until
/// the user has answered (the first-run prompt, or the Settings checkbox).
///
/// Unanswered is NOT consent. The startup check contacts GitHub, so it only
/// runs once the user said yes; until then clew asks ([`consent_task`]).
pub fn auto_check_preference() -> Option<bool> {
    clew_core::globalconfig::get("auto_update").and_then(|v| v.as_bool())
}

/// Whether clew checks for updates automatically at startup: only when the
/// user agreed (see [`auto_check_preference`]).
pub fn auto_check_enabled() -> bool {
    auto_check_preference() == Some(true)
}

/// The first-run question, as a native alert. Answers with
/// `UpdaterMsg::SetAuto`, which persists the choice, so it is asked once;
/// an alert closed without choosing leaves the question open for the next
/// launch (and checks nothing now).
pub fn consent_task() -> Task<Message> {
    const YES: &str = "Check Automatically";
    const NO: &str = "Don't Check";
    Task::perform(
        async {
            rfd::AsyncMessageDialog::new()
                .set_level(rfd::MessageLevel::Info)
                .set_title("Check for clew updates automatically?")
                .set_description(
                    "clew can ask GitHub for a newer release each time it starts. \
                     Nothing about your projects is sent. You can change this later in \
                     Settings, and check by hand from the clew menu at any time.",
                )
                .set_buttons(rfd::MessageButtons::OkCancelCustom(YES.into(), NO.into()))
                .show()
                .await
        },
        |answer| match answer {
            rfd::MessageDialogResult::Custom(label) if label == YES => {
                Message::Updater(UpdaterMsg::SetAuto(true))
            }
            rfd::MessageDialogResult::Custom(label) if label == NO => {
                Message::Updater(UpdaterMsg::SetAuto(false))
            }
            _ => Message::Noop,
        },
    )
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
        move |result| Message::Updater(UpdaterMsg::Checked { manual, result }),
    )
}

/// Where the release API may point an update download.
///
/// The URL comes out of a JSON document fetched over the network, and the
/// installer verifies whatever arrives, so this is not what makes an update
/// safe — it keeps clew from being told to fetch half a gigabyte from an
/// arbitrary host in the first place. (The transfer itself is HTTPS-only,
/// time-limited and capped at `clew_core::update::MAX_UPDATE_BYTES` by
/// `clew_core::net`.) Under the default release host only this project's own
/// release assets on github.com qualify; a self-hosted `CLEW_UPDATE_API`
/// names its own host, so there only the transport rule applies: https, or
/// plain http to a loopback mirror — exactly what `clew_core::net` accepts.
pub fn validate_dmg_url(url: &str) -> Result<(), String> {
    let overridden = std::env::var_os("CLEW_UPDATE_API").is_some();
    if overridden && clew_core::net::is_loopback_http(url) {
        return Ok(());
    }
    if !url.starts_with("https://") {
        return Err(format!("refusing a non-https update download: {url}"));
    }
    if overridden {
        return Ok(());
    }
    // Judged as the transfer will send it: the raw text's prefix passes
    // `…/releases/download/%2e%2e/%2e%2e/…`, which resolves elsewhere.
    let release_asset = clew_core::net::https_host_path(url).is_some_and(|(host, path)| {
        host == "github.com" && path.starts_with("/RutaTang/Clew/releases/download/")
    });
    if !release_asset {
        return Err(format!(
            "refusing an update download from outside clew's GitHub releases: {url}"
        ));
    }
    Ok(())
}

/// The file a failed swap leaves in `<data root>/updates` for the relaunched
/// clew to report (see `macos::install::swap_script`).
pub const INSTALL_FAILED_MARKER: &str = "last-update-failed";

/// The swap helper's own log, next to that marker.
pub const INSTALL_LOG: &str = "update.log";

/// The reason the previous update's swap failed, if it did — read once and
/// removed, so it is reported exactly once. Capped and opened without
/// following a symlink: it sits in a directory other processes write to.
pub fn take_install_failure() -> Option<String> {
    use std::io::Read;
    let path = updates_dir().ok()?.join(INSTALL_FAILED_MARKER);
    let file = clew_core::statefile::open_plain(&path)?;
    let mut text = String::new();
    let read = file.take(4096).read_to_string(&mut text);
    let _ = std::fs::remove_file(&path);
    read.ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
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
/// streaming into, and whichever `UpdaterMsg::Downloaded` landed first handed
/// `hdiutil` an image the other task was still writing. With a directory per
/// attempt neither run can see, truncate, or install the other's bytes.
///
/// The directory outlives the download, so every route that ends one has to
/// take it: [`discard_download`] runs on a failed download — an aborted one
/// included (Cancel, or its window closing), and one that finished just as it
/// was aborted — after an install attempt, on a generation the app has moved
/// past, and on a result that comes back for a window that has since closed.
/// What no runtime route can reach is a clew that stops existing mid-stream,
/// and since this is no longer the temp dir nothing else would ever clear
/// that — hence [`sweep_stale_downloads`] at launch.
fn create_download_dest(version: Version) -> Result<PathBuf, String> {
    Ok(create_private_dir(&updates_dir()?, DOWNLOAD_PREFIX)?.join(format!("Clew-{version}.dmg")))
}

/// Where every download attempt's directory lives. No data directory means no
/// safe place to write one, so this fails rather than falling back (see
/// [`create_download_dest`]).
fn updates_dir() -> Result<PathBuf, String> {
    updates_dir_under(clew_core::lsp::store::data_root())
}

/// [`updates_dir`] for the data root `root` — `None` when there is none.
fn updates_dir_under(root: Option<PathBuf>) -> Result<PathBuf, String> {
    root.map(|root| root.join("updates"))
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
/// [`discard_download`] covers every route that *ends* a download while clew
/// runs: a failure, an abort (Cancel, or its window closing — ⌘Q closes them
/// all), an install attempt, a superseded generation, a result for a window
/// that has closed or is quitting. What it cannot cover is a clew that stops
/// existing first — a crash, a log-out, AppKit terminating clew directly — and
/// a finished image whose result has left the download's stream (so the stream
/// no longer discards it) but is never handled, the process ending in between.
/// Since the destination moved off the temp dir nothing else clears those any
/// more: not the next launch, not a later successful update, and no longer the
/// reboot that used to empty `/tmp`. A whole release image would sit under the
/// data root for good. This is the missing sweep, run once per launch.
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
/// and a final `UpdaterMsg::Downloaded`. `generation` lets the handler drop a
/// superseded run's late messages.
///
/// The handle is how the download is stopped while clew runs: aborting it
/// drops the stream, which cancels the transfer ([`StopOnDrop`]) and takes the
/// attempt's directory with it. The app aborts it on Cancel and when the
/// window that started it closes — iced keeps draining a closed window's
/// streams, so without the abort the transfer ran to its end, up to two hours
/// on a slow link, only for its image to be thrown away.
pub fn download_task(
    url: String,
    version: Version,
    generation: u64,
) -> (Task<Message>, iced::task::Handle) {
    Task::run(download_stream(url, version, generation), |m| m).abortable()
}

/// The messages of [`download_task`], as a stream: dropping it stops the
/// download ([`StopOnDrop`]).
fn download_stream(
    url: String,
    version: Version,
    generation: u64,
) -> impl iced::futures::Stream<Item = Message> {
    use iced::futures::StreamExt;
    // Whether the finished image reached the app: answered as its
    // `Downloaded` leaves this stream (the `inspect` below), and dropped
    // unanswered with the stream when an abort gets there first.
    let (handed_over, taken) = std::sync::mpsc::sync_channel::<()>(1);
    iced::stream::channel(
        256,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            // Claimed when the download actually starts rather than when the
            // task is built: the destination is a directory now, and a task
            // dropped before it ever ran would leave one behind.
            let dest = match create_download_dest(version) {
                Ok(d) => d,
                Err(e) => {
                    let _ = output
                        .send(Message::Updater(UpdaterMsg::Downloaded {
                            generation,
                            result: Err(e),
                        }))
                        .await;
                    return;
                }
            };
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DlPiece>();
            let dl_dest = dest.clone();
            // Set once nobody is listening — this stream ended, or was dropped
            // with its task, which the app aborts on Cancel and when the
            // window that started it closes (see `download_task`) — so the
            // transfer stops too, rather than fetching an image nobody is
            // left to install.
            let stop = Arc::new(AtomicBool::new(false));
            let _stop_on_drop = StopOnDrop(stop.clone());
            // Blocking producer: download in chunks, forwarding throttled progress.
            tokio::task::spawn_blocking(move || {
                let mut last = Instant::now();
                let res = update::download_to(&url, &dl_dest, &stop, |done, total| {
                    if last.elapsed() >= Duration::from_millis(100) {
                        last = Instant::now();
                        let _ = tx.send(DlPiece::Progress(done, total));
                    }
                });
                match res {
                    Ok(()) => {
                        let _ = tx.send(DlPiece::Done(Ok(dl_dest.clone())));
                        // The image is the app's once its message is out, and
                        // until then still this attempt's. An abort landing
                        // after the last byte (the final flush to disk takes
                        // a while) drops the stream with the result still in
                        // it: nothing would carry the path anywhere, and the
                        // whole image would stay under the data root.
                        if taken.recv().is_err() {
                            discard_download(&dl_dest);
                        }
                    }
                    Err(e) => {
                        // A failed download leaves a partial image nobody will
                        // install — and this attempt's directory is nobody
                        // else's, so taking it with us costs no other run its
                        // bytes.
                        discard_download(&dl_dest);
                        let _ = tx.send(DlPiece::Done(Err(e)));
                    }
                }
            });
            // Drain the channel into UI messages.
            while let Some(piece) = rx.recv().await {
                let (msg, done) = match piece {
                    DlPiece::Progress(done, total) => (
                        Message::Updater(UpdaterMsg::DownloadProgress {
                            generation,
                            done,
                            total,
                        }),
                        false,
                    ),
                    DlPiece::Done(result) => (
                        Message::Updater(UpdaterMsg::Downloaded { generation, result }),
                        true,
                    ),
                };
                if output.send(msg).await.is_err() || done {
                    break;
                }
            }
        },
    )
    .inspect(move |msg| {
        if matches!(
            msg,
            Message::Updater(UpdaterMsg::Downloaded { result: Ok(_), .. })
        ) {
            let _ = handed_over.try_send(());
        }
    })
}

/// Sets its flag when dropped: how the download task tells its blocking
/// producer that nobody is waiting any more, however the task ends.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Verify the downloaded DMG, swap the bundle in, and launch the relauncher.
/// On success the app should quit so the detached helper can finish (the shell
/// does that on `UpdaterMsg::Installed(Ok)`). `expected` is the version the user was
/// offered — the image must hold exactly that; `reopen` is the project to
/// reopen after relaunch, if any. The download's own directory goes on either
/// outcome ([`discard_download`]) — a task that panics outright is the one case
/// that leaves it.
pub fn install_task(dmg: PathBuf, expected: Version, reopen: Option<PathBuf>) -> Task<Message> {
    Task::perform(
        async move {
            #[cfg(target_os = "macos")]
            {
                tokio::task::spawn_blocking(move || {
                    let done = crate::macos::install::install_dmg(&dmg, expected, reopen);
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
                let _ = (expected, reopen);
                discard_download(&dmg);
                Err("self-install is only supported on macOS".to_string())
            }
        },
        |v| Message::Updater(UpdaterMsg::Installed(v)),
    )
}

/// What the download tests share — here, and the App's (Cancel, a window
/// closing mid-download).
#[cfg(test)]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// A release host on loopback for `downloads` downloads: each gets a
    /// response head and the first byte of a longer body, then silence until
    /// its client hangs up or 20 s pass — so a download from it ends within
    /// seconds only by being cancelled. Returns the image's URL, and a
    /// receiver that yields once per download under way.
    pub(crate) fn stalling_host(downloads: usize) -> (String, std::sync::mpsc::Receiver<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/Clew.dmg", listener.local_addr().unwrap());
        let (served_tx, served) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for conn in listener.incoming().take(downloads) {
                let Ok(mut conn) = conn else { return };
                let served_tx = served_tx.clone();
                std::thread::spawn(move || {
                    use std::io::{Read, Write};
                    clew_core::testutil::read_http_request(&mut conn);
                    let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\nx");
                    let _ = served_tx.send(());
                    let _ = conn.set_read_timeout(Some(Duration::from_secs(20)));
                    let _ = conn.read(&mut [0u8; 1]);
                });
            }
        });
        (url, served)
    }

    /// The download attempts' directories under the data root `root`.
    pub(crate) fn download_dirs(root: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(root.join("updates"))
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    }

    /// Wait until `done` holds, failing with `what` after 5 s — well inside
    /// the 20 s [`stalling_host`] holds a download open for.
    pub(crate) async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{download_dirs, eventually, stalling_host};
    use super::*;
    use crate::app::tests::{EnvVars, data_dir_override, test_dir};
    use std::os::unix::fs::PermissionsExt;

    /// Every update directory — a download's and an install's alike — is its
    /// own, private to this user, and an entry that already exists is stepped
    /// over rather than adopted: neither a second window nor a pre-planted
    /// directory can be the one clew writes an update into.
    #[test]
    fn each_update_directory_is_fresh_private_and_never_adopted() {
        let parent = test_dir("privdir");

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
    }

    /// Two windows offered the same release both download it, and the second
    /// used to open the first's half-written `Clew-<version>.dmg` with
    /// `truncate` — zeroing bytes the first had already streamed, under an
    /// installer that could pick the file up at any moment. Each attempt must
    /// therefore get a destination the other cannot name, let alone read.
    #[test]
    fn two_download_attempts_never_share_a_file() {
        let root = test_dir("dl-race");
        let _env = data_dir_override(&root);

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
    }

    /// The download's directory is this attempt's alone, so it goes when its
    /// bytes stop being useful — but `install_task` is handed a path that came
    /// through a message, and this deletes a TREE. Only a directory the
    /// download path itself named may be removed.
    #[test]
    fn only_our_own_download_directory_is_ever_deleted() {
        let root = test_dir("dl-discard");
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
        let root = test_dir("dl-sweep");
        let _env = data_dir_override(&root);
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
    }

    /// Only this project's releases, over https, may be downloaded under the
    /// default release host.
    #[test]
    fn only_https_release_assets_of_this_project_are_downloaded() {
        let default_host = EnvVars::new().remove("CLEW_UPDATE_API");
        let ok = "https://github.com/RutaTang/Clew/releases/download/v0.2.0/Clew-0.2.0-arm64.dmg";
        assert!(validate_dmg_url(ok).is_ok());
        for bad in [
            "http://github.com/RutaTang/Clew/releases/download/v0.2.0/Clew.dmg",
            "https://evil.example/RutaTang/Clew/releases/download/v0.2.0/Clew.dmg",
            "https://github.com.evil.example/RutaTang/Clew/releases/download/x.dmg",
            "https://github.com/someone/else/releases/download/v1/Clew.dmg",
            "https://github.com@evil.example/RutaTang/Clew/releases/download/x.dmg",
            "ftp://github.com/x.dmg",
            // Dot segments, spelled so a prefix test on the text passes:
            // the URL parser resolves them to another repository's asset.
            "https://github.com/RutaTang/Clew/releases/download/%2e%2e/%2e%2e/%2e%2e/%2e%2e/x/y/releases/download/v1/Clew.dmg",
            "https://github.com/RutaTang/Clew/releases/download/../../../../x/y/releases/download/v1/Clew.dmg",
            "https://github.com:8443/RutaTang/Clew/releases/download/v1/Clew.dmg",
        ] {
            assert!(validate_dmg_url(bad).is_err(), "{bad}");
        }
        // A self-hosted release API names its own host, but https still holds.
        let _self_hosted = default_host.set("CLEW_UPDATE_API", "https://updates.example/api");
        assert!(validate_dmg_url("https://cdn.example/Clew.dmg").is_ok());
        assert!(validate_dmg_url("http://cdn.example/Clew.dmg").is_err());
        assert!(
            validate_dmg_url("http://127.0.0.1:8080/Clew.dmg").is_ok(),
            "a loopback mirror, as clew_core::net allows"
        );
    }

    /// Unanswered is not consent: with no `auto_update` in config.toml the
    /// startup check does not run (the first-run question is asked instead).
    #[test]
    fn the_automatic_check_waits_for_an_answer() {
        let root = test_dir("update-consent");
        std::fs::create_dir_all(&root).unwrap();
        let _env = data_dir_override(&root);

        assert_eq!(auto_check_preference(), None);
        assert!(
            !auto_check_enabled(),
            "no answer must mean no network check"
        );
        set_auto_check(true).unwrap();
        assert_eq!(auto_check_preference(), Some(true));
        assert!(auto_check_enabled());
        set_auto_check(false).unwrap();
        assert_eq!(auto_check_preference(), Some(false));
    }

    /// The running version is the bundle's `CFBundleShortVersionString` when
    /// there is one — what an update's own bundle version is compared with —
    /// and the crate version otherwise, or when the bundle's is unusable.
    #[test]
    fn the_running_version_is_the_bundles_else_the_crates() {
        let v = |s: &str| Version::parse(s).unwrap();
        assert_eq!(running_version(Some("1.2.3"), "0.1.13"), v("1.2.3"));
        assert_eq!(running_version(Some("0.1.13"), "0.1.13"), v("0.1.13"));
        assert_eq!(running_version(None, "0.1.13"), v("0.1.13"));
        assert_eq!(
            running_version(Some("not a version"), "0.1.13"),
            v("0.1.13")
        );
        assert_eq!(running_version(None, "garbage"), v("0.0.0"));
        // This test binary runs from no bundle.
        assert_eq!(bundle_short_version(), None);
        assert_eq!(current_version(), v(CLIENT_VERSION));
    }

    /// A failed swap's reason is reported by the relaunched clew exactly once.
    #[test]
    fn a_failed_install_is_reported_once_after_relaunch() {
        let root = test_dir("failmark");
        let _env = data_dir_override(&root);

        assert_eq!(take_install_failure(), None);
        let updates = updates_dir().unwrap();
        std::fs::create_dir_all(&updates).unwrap();
        std::fs::write(
            updates.join(INSTALL_FAILED_MARKER),
            "could not copy the new version into place\n",
        )
        .unwrap();
        assert_eq!(
            take_install_failure().as_deref(),
            Some("could not copy the new version into place")
        );
        assert_eq!(take_install_failure(), None, "reported once, then gone");
    }

    /// The one place a downloaded update may be written is clew's own data
    /// directory. Falling back to the shared temp dir when there is none was
    /// the exact weakness the destination was moved off /tmp to avoid: a
    /// predictable name another local user can pre-plant as a symlink.
    ///
    /// "No data directory" is asked of `updates_dir_under` directly: unsetting
    /// `CLEW_DATA_DIR` and `HOME` for the process, as this test used to, took
    /// the data root away from every test running beside it.
    #[test]
    fn a_download_without_a_data_directory_fails_instead_of_using_temp() {
        let root = test_dir("download-dest");
        let _env = data_dir_override(&root);
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
        let err = updates_dir_under(None).expect_err("no data directory must fail the download");
        assert!(err.contains("no data directory"), "{err}");
    }

    /// A download nobody listens to any more stops: dropping its stream —
    /// what aborting its task does, on Cancel and when the window that
    /// started it closes — cancels the transfer, and its failure takes the
    /// attempt's directory with it. It used to run to its end — up to two
    /// hours on a slow link — for nobody. (The aborts themselves are tested
    /// where they are made: the App's Cancel, and the shell's window close.)
    #[tokio::test]
    async fn dropping_the_download_stops_it() {
        use iced::futures::StreamExt;
        let root = test_dir("dl-stop");
        let _env = data_dir_override(&root);
        let (url, served) = stalling_host(1);
        let version = Version::parse("9.9.9").unwrap();
        let mut stream = Box::pin(download_stream(url, version, 1));
        // Drive the task until the download is under way (progress is
        // throttled, so nothing need come out of the stream yet).
        let _ = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
        served
            .recv_timeout(Duration::from_secs(10))
            .expect("the download never started");
        assert_eq!(download_dirs(&root).len(), 1);
        drop(stream);
        eventually("the dropped download kept running", || {
            download_dirs(&root).is_empty()
        })
        .await;
    }

    /// An abort can land after the last byte: the transfer is over (its final
    /// flush to disk takes a while) but its `Downloaded` has not left the
    /// stream yet. Dropping the stream then dropped that result with it, and
    /// with it the only thing that carried the image's path: nothing removed
    /// the image, and a whole release sat under the data root until some
    /// later launch's sweep. The image is the app's only once its message is
    /// out.
    #[tokio::test]
    async fn a_download_aborted_after_its_last_byte_leaves_nothing_behind() {
        use iced::futures::StreamExt;
        use std::io::{Read, Write};
        let root = test_dir("dl-late-abort");
        let _env = data_dir_override(&root);
        // A host that sends the head, and the body's one byte when told to.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (served_tx, served) = std::sync::mpsc::channel();
        let (last_byte, send_it) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                clew_core::testutil::read_http_request(&mut s);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n");
                let _ = served_tx.send(());
                if send_it.recv_timeout(Duration::from_secs(20)).is_ok() {
                    let _ = s.write_all(b"x");
                }
                let _ = s.set_read_timeout(Some(Duration::from_secs(20)));
                let _ = s.read(&mut [0u8; 1]);
            }
        });
        let version = Version::parse("9.9.9").unwrap();
        let url = format!("http://{addr}/Clew.dmg");
        let mut stream = Box::pin(download_stream(url, version, 1));
        let _ = tokio::time::timeout(Duration::from_millis(300), stream.next()).await;
        served
            .recv_timeout(Duration::from_secs(10))
            .expect("the download never started");
        let dirs = download_dirs(&root);
        assert_eq!(dirs.len(), 1);
        let image = dirs[0].join("Clew-9.9.9.dmg");
        // The last byte, and time for the transfer to finish, all without
        // polling the stream: its result stays in it.
        last_byte.send(()).unwrap();
        eventually("the last byte never landed", || {
            std::fs::metadata(&image).is_ok_and(|m| m.len() == 1)
        })
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        drop(stream);
        eventually(
            "an image finished as its download was aborted was kept",
            || !dirs[0].exists(),
        )
        .await;
    }
}
