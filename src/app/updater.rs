//! Auto-update handlers: driving the check → download → verify → install →
//! relaunch flow, holding it together with the [`UpdateState`] in the App. The
//! async tasks live in `crate::updater`; the macOS bundle swap in
//! `crate::macos::install`.

use crate::app::prelude::*;
use crate::*;

impl App {
    /// The silent startup check, once per process and only if enabled.
    pub(crate) fn startup_update_check(&self) -> Task<Message> {
        if self.update.auto_check && updater::claim_startup_check() {
            updater::check_task(false)
        } else {
            Task::none()
        }
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
                // when nothing is running. The periodic check fires on a timer,
                // so it lands mid-download routinely, and resetting the phase
                // there re-armed the install button underneath a download that
                // was still going: a second press started a second download of
                // the same image and orphaned the first. The generation counter
                // makes the orphan harmless (its completion is dropped at
                // `on_update_downloaded`), so this is wasted bandwidth rather
                // than a bad install, but there is no reason to allow it.
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
        // In-app install needs a DMG and a real installed bundle to swap. Without
        // either (no asset, or a dev build) fall back to the release page.
        match (&update.dmg_url, self.can_self_install()) {
            (Some(url), true) => {
                let url = url.clone();
                let version = update.version;
                self.update.generation += 1;
                self.update.phase = UpdatePhase::Downloading;
                self.update.progress = Some((0, None));
                self.update.show_notes = false;
                self.status = format!("Downloading clew {version}…");
                updater::download_task(url, version, self.update.generation)
            }
            _ => {
                let url = updater::release_page_url(&update.version);
                Task::done(Message::OpenLink(url))
            }
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
        match result {
            Ok(dmg) => {
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
                    .then(|| self.project.as_ref().map(|p| p.root.clone()))
                    .flatten();
                updater::install_task(dmg, reopen)
            }
            Err(e) => {
                self.update.phase = UpdatePhase::Failed(e.clone());
                self.update.progress = None;
                self.status = format!("Update download failed: {e}");
                Task::none()
            }
        }
    }

    /// The bundle swap + relauncher finished.
    pub(crate) fn on_update_installed(&mut self, result: Result<(), String>) -> Task<Message> {
        match result {
            // The detached helper is waiting for us to exit before it swaps the
            // bundle and relaunches, so quit now — but through the same
            // teardown every other quit takes. This is a quit like any other:
            // a bare `iced::exit()` here left the debug adapter and its
            // debuggee running while the app was replaced underneath them.
            Ok(()) => crate::shell::exit_after(self.stop_debug_session()),
            Err(e) => {
                self.update.phase = UpdatePhase::Failed(e.clone());
                self.status = format!("Update failed: {e}");
                Task::none()
            }
        }
    }

    /// Toggle the persisted "check for updates automatically" preference.
    pub(crate) fn on_set_auto_update(&mut self, enabled: bool) -> Task<Message> {
        self.update.auto_check = enabled;
        if let Err(e) = updater::set_auto_check(enabled) {
            self.status = format!("Could not save preference: {e}");
        }
        Task::none()
    }

    /// Whether clew can replace its own bundle: macOS, running from an installed
    /// `.app` that carries a signing team an update can be anchored to.
    ///
    /// Both halves are needed BEFORE the download. Checking only the `.app`
    /// meant an ad-hoc-signed install pulled the whole disk image down and then
    /// failed at the first line of `install_dmg`; it now falls through to the
    /// release page instead, like any other build that cannot swap itself.
    #[cfg(target_os = "macos")]
    fn can_self_install(&self) -> bool {
        crate::macos::install::self_install_supported()
    }
    #[cfg(not(target_os = "macos"))]
    fn can_self_install(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A download whose generation has moved on is one nobody will ever
    /// install: the periodic check can start a second attempt over the first,
    /// and the first still finishes. Returning without its bytes stranded a
    /// whole release image in a per-attempt directory that nothing else ever
    /// visits — the same leak as an orphaned window's, by the same argument.
    #[test]
    fn a_superseded_download_leaves_nothing_under_the_data_root() {
        let root = std::env::temp_dir().join(format!("clew-superseded-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = updater::create_private_dir(&root, updater::DOWNLOAD_PREFIX).unwrap();
        let dmg = dir.join("Clew-9.9.9.dmg");
        std::fs::write(&dmg, b"image").unwrap();

        let mut app = App::blank();
        app.update.generation = 7;
        let _ = app.on_update_downloaded(6, Ok(dmg));

        assert!(!dir.exists(), "a superseded attempt's image was kept");
        assert_eq!(
            app.update.phase,
            UpdatePhase::Idle,
            "a superseded result must not move the live attempt's phase"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
