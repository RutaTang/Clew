//! Auto-update orchestration: the async tasks that check for, download, and
//! install a new clew release.
//!
//! The pure logic (finding the latest release, streaming the download) lives in
//! `clew_core::update`; the macOS bundle swap lives in `crate::macos::install`.
//! This module wires them to the iced update loop as `Task`s. The App handlers
//! that drive these tasks and hold the state live in `crate::app::updater`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

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

/// Streamed download of the DMG at `url`, emitting throttled progress messages
/// and a final `UpdateDownloaded`. `generation` lets the handler drop a
/// superseded run's late messages.
pub fn download_task(url: String, version: Version, generation: u64) -> Task<Message> {
    let dest = std::env::temp_dir().join(format!("Clew-{version}.dmg"));
    let stream = iced::stream::channel(
        256,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
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
/// the project to reopen after relaunch, if any.
pub fn install_task(dmg: PathBuf, reopen: Option<PathBuf>) -> Task<Message> {
    Task::perform(
        async move {
            #[cfg(target_os = "macos")]
            {
                tokio::task::spawn_blocking(move || {
                    crate::macos::install::install_dmg(&dmg, reopen)
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (dmg, reopen);
                Err("self-install is only supported on macOS".to_string())
            }
        },
        Message::UpdateInstalled,
    )
}
