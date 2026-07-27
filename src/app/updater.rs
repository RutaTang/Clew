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
                // A fresh find clears any earlier failure / progress.
                self.update.phase = UpdatePhase::Idle;
                self.update.progress = None;
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
            return Task::none(); // superseded
        }
        match result {
            Ok(dmg) => {
                self.update.phase = UpdatePhase::Installing;
                self.update.progress = None;
                self.status = "Installing update…".into();
                let reopen = self.project.as_ref().map(|p| p.root.clone());
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
            // bundle and relaunches, so quit now.
            Ok(()) => iced::exit(),
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
    /// `.app`.
    #[cfg(target_os = "macos")]
    fn can_self_install(&self) -> bool {
        crate::macos::install::installed_bundle().is_some()
    }
    #[cfg(not(target_os = "macos"))]
    fn can_self_install(&self) -> bool {
        false
    }
}
