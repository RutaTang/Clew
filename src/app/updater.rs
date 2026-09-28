//! Auto-update handlers: driving the check → download → verify → install →
//! relaunch flow, holding it together with the [`UpdateState`] in the App. The
//! async tasks live in `crate::updater`; the macOS bundle swap in
//! `crate::macos::install`.
//!
//! Its messages, [`UpdaterMsg`], arrive through `App::update_updater`.

use crate::app::prelude::*;
use crate::*;

impl App {
    /// Startup work for the updater, once per process (the first window).
    ///
    /// - A swap that failed after the last run quit for it is reported now:
    ///   the helper relaunched the previous version and left the reason
    ///   behind, and this is the first moment anyone can show it.
    /// - The automatic check runs only with the user's consent. Unanswered,
    ///   the first-run question is asked instead of contacting GitHub.
    pub(crate) fn startup_update_check(&self) -> Task<Message> {
        if !updater::claim_startup_check() {
            return Task::none();
        }
        let failure = match updater::take_install_failure() {
            Some(reason) => Task::done(Message::Updater(UpdaterMsg::Installed(Err(reason)))),
            None => Task::none(),
        };
        let check = match updater::auto_check_preference() {
            Some(true) => updater::check_task(false),
            Some(false) => Task::none(),
            None => updater::consent_task(),
        };
        Task::batch([failure, check])
    }

    /// A manual "Check for Updates" (menu), which announces the result either
    /// way.
    pub(crate) fn on_check_for_updates(&mut self) -> Task<Message> {
        if self.update.checking {
            return Task::none();
        }
        self.update.checking = true;
        self.status = "Checking for updates…".into();
        updater::check_task(true)
    }

    /// A version check finished.
    pub(crate) fn on_update_checked(
        &mut self,
        manual: bool,
        result: Result<clew_core::update::Release, String>,
    ) -> Task<Message> {
        self.update.checking = false;
        match result {
            Ok(release) if release.version > updater::current_version() => {
                let notes = iced::widget::markdown::parse(&release.notes).collect();
                self.update.available = Some(AvailableUpdate {
                    version: release.version,
                    dmg_url: release.dmg_url,
                    notes,
                });
                // A fresh find clears any earlier failure / progress — but only
                // when nothing is running. A check can land mid-download (the
                // startup check racing a download started from an earlier
                // find, or a manual "Check for Updates" pressed during one),
                // and resetting the phase there re-armed the install button
                // underneath a download that was still going: a second press
                // started a second download of the same image and orphaned the
                // first. The generation counter makes the orphan harmless (its
                // completion is dropped at `on_update_downloaded`), so this is
                // wasted bandwidth rather than a bad install, but there is no
                // reason to allow it.
                if !matches!(
                    self.update.phase,
                    UpdatePhase::Downloading | UpdatePhase::Installing
                ) {
                    self.update.phase = UpdatePhase::Idle;
                    self.update.progress = None;
                }
                if manual {
                    self.status = format!("clew {} is available", release.version);
                }
            }
            Ok(_) => {
                if manual {
                    self.status = format!("clew is up to date (v{})", updater::current_version());
                }
            }
            Err(e) => {
                if manual {
                    self.status = format!("Update check failed: {e}");
                }
            }
        }
        Task::none()
    }

    /// Begin downloading and installing the available update.
    pub(crate) fn on_update_install_start(&mut self) -> Task<Message> {
        let Some(update) = self.update.available.as_ref() else {
            return Task::none();
        };
        // Second line of defence for the same race the phase reset above now
        // avoids: any other path that leaves the button pressable while a
        // download or install is running must not start a second one. This is
        // per-App and therefore per-WINDOW — two windows still download
        // independently, each into its own destination directory, and the swap
        // helper each would spawn works from its own staging copy, so the
        // outcome is duplicated work rather than a corrupted install. Making it
        // one download across windows needs shared state this struct does not
        // have.
        if matches!(
            self.update.phase,
            UpdatePhase::Downloading | UpdatePhase::Installing
        ) {
            return Task::none();
        }
        let release_page = updater::release_page_url(&update.version);
        let Some(url) = update.dmg_url.clone() else {
            // No image attached: the release page is the only way.
            return Task::done(Message::Content(ContentMsg::OpenLink(release_page)));
        };
        // The link came out of a network response; it is not fetched unless
        // it points at this project's releases, over https.
        if let Err(e) = updater::validate_dmg_url(&url) {
            self.status = format!("Not downloading the update ({e}) — opening the release page");
            return Task::done(Message::Content(ContentMsg::OpenLink(release_page)));
        }
        // In-app install needs a bundle this account can replace and a signing
        // team to anchor the update to. Without either (a dev build, a
        // disk-image or translocated copy, a read-only /Applications) say why
        // and fall back to the release page — BEFORE the download, rather than
        // after clew has quit for a swap that cannot happen.
        if let Some(reason) = self.self_install_blocker() {
            self.status =
                format!("clew can't update itself here: {reason} — opening the release page");
            return Task::done(Message::Content(ContentMsg::OpenLink(release_page)));
        }
        let version = update.version;
        self.start_update_download(url, version)
    }

    /// Download `version`'s image from `url`, which the checks in
    /// [`Self::on_update_install_start`] have passed, keeping the handle that
    /// stops it ([`Self::abort_update_download`]).
    pub(crate) fn start_update_download(
        &mut self,
        url: String,
        version: clew_core::update::Version,
    ) -> Task<Message> {
        // One download per window: a handle dropped unaborted would leave its
        // transfer with nothing left to stop it.
        self.abort_update_download();
        self.update.generation += 1;
        self.update.phase = UpdatePhase::Downloading;
        self.update.progress = Some((0, None));
        self.update.show_notes = false;
        self.status = format!("Downloading clew {version}…");
        let (task, download) = updater::download_task(url, version, self.update.generation);
        self.update.download = Some(download);
        task
    }

    /// The banner's Cancel: stop the download and go back to the offer, from
    /// which "Update now" starts afresh.
    ///
    /// The generation moves on too, so a result that was already on its way —
    /// a transfer that ended just as Cancel was pressed — lands as a
    /// superseded one: its image is discarded ([`Self::on_update_downloaded`])
    /// rather than installed, and a failure is not reported over the offer.
    pub(crate) fn on_update_download_cancel(&mut self) -> Task<Message> {
        if self.update.phase != UpdatePhase::Downloading {
            return Task::none();
        }
        self.abort_update_download();
        self.update.generation += 1;
        self.update.phase = UpdatePhase::Idle;
        self.update.progress = None;
        self.status = "Update download cancelled".into();
        Task::none()
    }

    /// Stop the download this window started, if one is running. Aborting its
    /// task drops the download's stream, which cancels the transfer and takes
    /// the attempt's directory with it (see `updater::download_task`).
    pub(crate) fn abort_update_download(&mut self) {
        if let Some(download) = self.update.download.take() {
            download.abort();
        }
    }

    /// Streamed download progress, guarded against a superseded run.
    pub(crate) fn on_update_download_progress(
        &mut self,
        generation: u64,
        done: u64,
        total: Option<u64>,
    ) -> Task<Message> {
        if generation == self.update.generation && self.update.phase == UpdatePhase::Downloading {
            self.update.progress = Some((done, total));
        }
        Task::none()
    }

    /// The DMG finished downloading; verify + swap it in next.
    pub(crate) fn on_update_downloaded(
        &mut self,
        generation: u64,
        result: Result<PathBuf, String>,
    ) -> Task<Message> {
        if generation != self.update.generation {
            // Superseded, so nothing will ever install these bytes. The
            // attempt's directory is this run's alone and nothing else visits
            // `<data root>/updates`, so returning here without it would strand
            // a whole release image for good.
            if let Ok(dmg) = &result {
                updater::discard_download(dmg);
            }
            return Task::none();
        }
        // This attempt is over: there is nothing left for its handle to stop.
        self.update.download = None;
        let expected = self.update.available.as_ref().map(|u| u.version);
        match (result, expected) {
            // clew is quitting — ⌘Q, or another window's installed update.
            // An install started now runs while the process goes, and its
            // helper relaunches clew, or a second helper is spawned: these
            // bytes are never installed.
            (Ok(dmg), _) if self.quitting => {
                updater::discard_download(&dmg);
                Task::none()
            }
            (Ok(dmg), Some(expected)) => {
                self.update.phase = UpdatePhase::Installing;
                self.update.progress = None;
                self.status = "Installing update…".into();
                // Only a LOCAL project can be reopened by path. A remote
                // project's root names the other host, so passing it would
                // have the relaunched clew open whatever THIS machine has
                // there — walking, indexing and explaining an unrelated tree
                // under the name of the user's remote project.
                let reopen = self
                    .local_project_state()
                    .then(|| self.proj.project.as_ref().map(|p| p.root.clone()))
                    .flatten();
                // The image must hold exactly the version that was offered.
                updater::install_task(dmg, expected, reopen)
            }
            (Ok(dmg), None) => {
                // Nothing on offer any more, so nothing to check the image
                // against: it is not installed.
                updater::discard_download(&dmg);
                let e = "the offered update is no longer known".to_string();
                self.update.phase = UpdatePhase::Failed(e.clone());
                self.status = format!("Update failed: {e}");
                Task::none()
            }
            (Err(e), _) => {
                self.update.phase = UpdatePhase::Failed(e.clone());
                self.update.progress = None;
                self.status = format!("Update download failed: {e}");
                Task::none()
            }
        }
    }

    /// The bundle swap + relauncher finished.
    ///
    /// On success the detached helper is waiting for this PROCESS to exit, and
    /// the quit is the whole app's: the shell intercepts `UpdaterMsg::Installed(Ok)`
    /// and runs `Shell::Quit` — every window's teardown, like ⌘Q. Exiting from
    /// here tore down this window's work alone and left the other windows'
    /// debug adapters and debuggees running while the app was replaced.
    pub(crate) fn on_update_installed(&mut self, result: Result<(), String>) -> Task<Message> {
        match result {
            Ok(()) => {
                self.status = "Restarting clew to finish the update…".into();
                Task::none()
            }
            Err(e) => {
                self.update.phase = UpdatePhase::Failed(e.clone());
                self.status = format!("Update failed: {e}");
                Task::none()
            }
        }
    }

    /// Record the "check for updates automatically" answer — from the
    /// first-run question or the Settings checkbox. Saying yes checks right
    /// away: that is what the user just agreed to, and the startup check this
    /// run already passed on.
    pub(crate) fn on_set_auto_update(&mut self, enabled: bool) -> Task<Message> {
        self.update.auto_check = enabled;
        if let Err(e) = updater::set_auto_check(enabled) {
            self.status = format!("Could not save preference: {e}");
        }
        if enabled && !self.update.checking && self.update.available.is_none() {
            updater::check_task(false)
        } else {
            Task::none()
        }
    }

    /// Why clew cannot replace its own bundle here, or `None` when it can:
    /// macOS, an installed `.app` this account may replace, signed by a team
    /// an update can be anchored to (see `macos::install::self_install_blocker`).
    #[cfg(target_os = "macos")]
    fn self_install_blocker(&self) -> Option<String> {
        crate::macos::install::self_install_blocker()
    }
    #[cfg(not(target_os = "macos"))]
    fn self_install_blocker(&self) -> Option<String> {
        Some("self-install is only supported on macOS".into())
    }
}

impl App {
    /// Handle a [`UpdaterMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_updater(&mut self, message: UpdaterMsg) -> Task<Message> {
        match message {
            // -- Auto-update -----------------------------------------------
            UpdaterMsg::Check => self.on_check_for_updates(),
            UpdaterMsg::Checked { manual, result } => self.on_update_checked(manual, result),
            UpdaterMsg::BannerDismissed => {
                self.update.available = None;
                self.update.phase = UpdatePhase::Idle;
                Task::none()
            }
            UpdaterMsg::ShowNotes => {
                self.update.show_notes = true;
                Task::none()
            }
            UpdaterMsg::CloseNotes => {
                self.update.show_notes = false;
                Task::none()
            }
            UpdaterMsg::InstallStart => self.on_update_install_start(),
            UpdaterMsg::CancelDownload => self.on_update_download_cancel(),
            UpdaterMsg::DownloadProgress {
                generation,
                done,
                total,
            } => self.on_update_download_progress(generation, done, total),
            UpdaterMsg::Downloaded { generation, result } => {
                self.on_update_downloaded(generation, result)
            }
            UpdaterMsg::Installed(result) => self.on_update_installed(result),
            UpdaterMsg::SetAuto(enabled) => self.on_set_auto_update(enabled),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{EnvVars, blank_app, test_dir};

    fn offered(url: &str) -> AvailableUpdate {
        AvailableUpdate {
            version: clew_core::update::Version {
                major: 9,
                minor: 9,
                patch: 9,
            },
            dmg_url: Some(url.into()),
            notes: Vec::new(),
        }
    }

    /// A download whose generation has moved on is one nobody will ever
    /// install: a second check can start a second attempt over the first,
    /// and the first still finishes. Returning without its bytes stranded a
    /// whole release image in a per-attempt directory that nothing else ever
    /// visits — the same leak as an orphaned window's, by the same argument.
    #[test]
    fn a_superseded_download_leaves_nothing_under_the_data_root() {
        let root = test_dir("superseded-dl");
        let dir = updater::create_private_dir(&root, updater::DOWNLOAD_PREFIX).unwrap();
        let dmg = dir.join("Clew-9.9.9.dmg");
        std::fs::write(&dmg, b"image").unwrap();

        let mut app = blank_app();
        app.update.generation = 7;
        let _ = app.on_update_downloaded(6, Ok(dmg));

        assert!(!dir.exists(), "a superseded attempt's image was kept");
        assert_eq!(
            app.update.phase,
            UpdatePhase::Idle,
            "a superseded result must not move the live attempt's phase"
        );
    }

    /// A closing window supersedes its download as Cancel does, and a
    /// quitting one installs nothing. ⌘Q closes every window but keeps each
    /// `App` through the teardown, so a result already on its way — past the
    /// download's own hand-over, which leaves its image to the app — still
    /// lands on the window, and matching the run, it started an install
    /// while clew quit. Now the closed run's image is discarded and its
    /// failure goes unreported, and whatever lands on a quitting window is
    /// never installed.
    #[test]
    fn a_download_that_lands_as_its_window_closes_is_never_installed() {
        let image = |tag: &str| {
            let dir =
                updater::create_private_dir(&test_dir(tag), updater::DOWNLOAD_PREFIX).unwrap();
            let dmg = dir.join("Clew-9.9.9.dmg");
            std::fs::write(&dmg, b"image").unwrap();
            (dir, dmg)
        };
        let url = "https://github.com/RutaTang/Clew/releases/download/v9.9.9/Clew-9.9.9-arm64.dmg";
        let lands = |app: &mut App, generation, result| {
            app.update(Message::Updater(UpdaterMsg::Downloaded {
                generation,
                result,
            }))
        };
        let mut app = blank_app();
        app.update.available = Some(offered(url));
        let version = app.update.available.as_ref().unwrap().version;
        // Under way. Its task never runs here: its results are delivered.
        let _transfer = app.start_update_download(url.into(), version);
        let closed = app.update.generation;

        let _ = app.on_window_closed();

        let _ = lands(&mut app, closed, Err("download cancelled".into()));
        assert_eq!(
            app.update.phase,
            UpdatePhase::Downloading,
            "the closed run's failure was reported"
        );
        let (dir, dmg) = image("closed-window-dl");
        let install = lands(&mut app, closed, Ok(dmg));
        assert_eq!(install.units(), 0, "an install started as clew quit");
        assert_ne!(app.update.phase, UpdatePhase::Installing);
        assert!(!dir.exists(), "the closed run's image was kept");

        // A download that lands on the quitting window, whatever its run —
        // here one begun as the window closed.
        let _transfer = app.start_update_download(url.into(), version);
        let current = app.update.generation;
        let (dir, dmg) = image("quitting-window-dl");
        let install = lands(&mut app, current, Ok(dmg));
        assert_eq!(install.units(), 0, "an install started as clew quit");
        assert_ne!(app.update.phase, UpdatePhase::Installing);
        assert!(!dir.exists(), "an image nobody will install was kept");
    }

    /// Cancel stops a download while clew runs, which nothing could before:
    /// the transfer stops and takes the attempt's directory with it, the
    /// aborted run delivers nothing, and the banner is back at the offer,
    /// from which "Update now" starts afresh. A failure of the cancelled run
    /// that was already on its way does not replace the offer either.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_stops_the_download_and_returns_to_the_offer() {
        use crate::app::tests::{data_dir_override, run_task_async};
        use crate::updater::testing::{download_dirs, eventually, stalling_host};
        use std::time::Duration;
        let root = test_dir("cancel-dl");
        let _env = data_dir_override(&root);
        let (url, served) = stalling_host(1);
        let mut app = blank_app();
        app.update.available = Some(offered(&url));
        let version = app.update.available.as_ref().unwrap().version;
        let run = tokio::spawn(run_task_async(app.start_update_download(url, version)));
        served
            .recv_timeout(Duration::from_secs(10))
            .expect("the download never started");
        let dirs = download_dirs(&root);
        assert_eq!(dirs.len(), 1);
        let cancelled = app.update.generation;

        let _ = app.update(Message::Updater(UpdaterMsg::CancelDownload));

        eventually("the cancelled download kept running", || !dirs[0].exists()).await;
        let delivered = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("the cancelled download's task kept running")
            .unwrap();
        assert!(
            !delivered
                .iter()
                .any(|m| matches!(m, Message::Updater(UpdaterMsg::Downloaded { .. }))),
            "{delivered:?}"
        );
        assert_eq!(app.update.phase, UpdatePhase::Idle);
        assert_eq!(app.update.progress, None);
        assert!(app.update.download.is_none());
        assert!(app.update.available.is_some(), "the offer is gone");
        assert!(app.status.contains("cancelled"), "{}", app.status);

        let _ = app.update(Message::Updater(UpdaterMsg::Downloaded {
            generation: cancelled,
            result: Err("download cancelled".into()),
        }));
        assert_eq!(
            app.update.phase,
            UpdatePhase::Idle,
            "the cancelled run's failure replaced the offer"
        );
    }

    /// A link that is not this project's release asset is never fetched; the
    /// user is sent to the release page with the reason.
    #[test]
    fn a_download_link_off_the_release_host_is_not_fetched() {
        let _env = EnvVars::new().remove("CLEW_UPDATE_API");

        let mut app = blank_app();
        app.update.available = Some(offered("https://evil.example/Clew.dmg"));
        let generation = app.update.generation;
        let _ = app.on_update_install_start();

        assert_eq!(app.update.generation, generation, "no download may start");
        assert_eq!(app.update.phase, UpdatePhase::Idle);
        assert!(app.status.contains("Not downloading"), "{}", app.status);
    }

    /// A copy that cannot replace itself (here: a test binary, not an
    /// installed app) says why instead of downloading an image it can never
    /// install.
    #[test]
    fn a_copy_that_cannot_swap_itself_says_why_before_downloading() {
        let mut app = blank_app();
        app.update.available = Some(offered(
            "https://github.com/RutaTang/Clew/releases/download/v9.9.9/Clew-9.9.9-arm64.dmg",
        ));
        let generation = app.update.generation;
        let _ = app.on_update_install_start();
        assert_eq!(app.update.generation, generation);
        assert!(app.status.contains("can't update itself"), "{}", app.status);
    }

    /// A successful install does not exit from inside one window: the quit is
    /// the shell's (see `shell::update`), so every window's teardown runs.
    #[test]
    fn an_installed_update_leaves_the_quit_to_the_shell() {
        let mut app = blank_app();
        let _ = app.on_update_installed(Ok(()));
        assert!(app.status.contains("Restarting"), "{}", app.status);
        let _ = app.on_update_installed(Err("disk full".into()));
        assert_eq!(app.update.phase, UpdatePhase::Failed("disk full".into()));
    }

    /// An image that arrives after its offer vanished has nothing to be
    /// checked against, so it is dropped rather than installed.
    #[test]
    fn an_image_with_no_offer_to_match_is_not_installed() {
        let root = test_dir("no-offer-dl");
        let dir = updater::create_private_dir(&root, updater::DOWNLOAD_PREFIX).unwrap();
        let dmg = dir.join("Clew-9.9.9.dmg");
        std::fs::write(&dmg, b"image").unwrap();

        let mut app = blank_app();
        let _ = app.on_update_downloaded(app.update.generation, Ok(dmg));
        assert!(!dir.exists());
        assert!(matches!(app.update.phase, UpdatePhase::Failed(_)));
    }
}
