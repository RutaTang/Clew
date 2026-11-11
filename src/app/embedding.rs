//! Embeddings made on this computer ([`clew_core::embed::local`]): the
//! choice between them and a service, and EmbeddingGemma's download and
//! removal — fetched only when asked for, its size and its terms said first,
//! each file checked as it lands.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clew_core::embed::local::{self, MODEL, ModelFile};
use clew_voice::models::{Checksum, Expected, Install};

use crate::app::prelude::*;
use crate::app::voice::{Claim, ClaimedFor, ModelDownload, StopOnDrop};
use crate::*;

/// EmbeddingGemma's download, while it runs, and why the last one failed;
/// and whether the program that runs it is here, as last looked for
/// (`App::refresh_embeddings`), not as the window is drawn.
#[derive(Default)]
pub(crate) struct LocalEmbedding {
    pub(crate) download: Option<ModelDownload>,
    pub(crate) error: Option<String>,
    pub(crate) program: bool,
    /// When the last download finished: for a moment its row says it is
    /// ready where Cancel was, not Remove — a click meant for Cancel
    /// deleted the model just fetched.
    pub(crate) downloaded_at: Option<std::time::Instant>,
}

/// How long a model just downloaded shows "Ready" where its Cancel was,
/// before Remove takes the place.
pub(crate) const JUST_DOWNLOADED: std::time::Duration = std::time::Duration::from_secs(2);

/// What a download of `file` must turn out to be.
fn expected(file: &ModelFile) -> Expected {
    Expected {
        name: file.path.to_string(),
        bytes: Some(file.bytes),
        max_bytes: file.bytes,
        checksum: Checksum::Sha256(file.sha256),
    }
}

/// Whether `path` is a file (not a folder, not a link).
fn is_file(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

/// What is missing to make embeddings on this computer, said as what to
/// do: the model to download, else the program that runs it, to install
/// beside Clew.
pub(crate) fn missing_here() -> String {
    if local::installed() {
        tr!("clew-speak, which makes embeddings on this computer, is not installed beside Clew")
            .into()
    } else {
        tr!(
            "Download {model} in Settings → Embeddings",
            model = MODEL.name
        )
    }
}

/// Why no embeddings can be made with what is saved, said as what to do:
/// what is missing on this computer, or a service's endpoint to set.
pub(crate) fn why_no_embeddings() -> String {
    if embed::Config::stored_here() {
        missing_here()
    } else {
        tr!("Configure an embedding endpoint in Settings").into()
    }
}

impl App {
    /// Whether EmbeddingGemma is here, and not being downloaded or removed
    /// in any window.
    pub(crate) fn embed_model_installed(&self) -> bool {
        local::dir()
            .is_some_and(|dir| !crate::app::voice::downloading(&dir) && local::present(&dir))
    }

    /// Whether embeddings can be made with what is saved, and if not, why
    /// — read again after anything that changes either, not as the window
    /// is drawn.
    pub(crate) fn refresh_embeddings(&mut self) {
        self.embed_available = embed::Config::available();
        self.embed_missing = why_no_embeddings();
        self.local_embedding.program = local::program().is_some();
    }

    /// The embedding settings and every window's use of them, after the
    /// model came or went: what is available follows, and the server is
    /// told.
    fn embed_model_changed(&mut self) {
        self.refresh_embeddings();
        self.send_ai_config();
    }

    /// Download EmbeddingGemma, file by file, each verified as it lands.
    pub(crate) fn start_embed_download(&mut self) -> Task<Message> {
        if self.local_embedding.download.is_some() {
            return Task::none();
        }
        let Some(dir) = local::dir() else {
            self.status = tr!("There is no data directory to keep the model in").into();
            return Task::none();
        };
        let claim = match Claim::take(dir.clone(), ClaimedFor::Download) {
            Ok(claim) => claim,
            Err(held) => {
                self.status = held.busy(MODEL.name);
                return Task::none();
            }
        };
        self.local_embedding.error = None;
        let missing: Vec<&'static ModelFile> = MODEL
            .files
            .iter()
            .filter(|f| !is_file(&dir.join(f.path)))
            .collect();
        let present: u64 = MODEL.bytes() - missing.iter().map(|f| f.bytes).sum::<u64>();
        let cancel = Arc::new(AtomicBool::new(false));
        let fetch = self.voice.fetch;
        let stream = iced::stream::channel(64, {
            let cancel = cancel.clone();
            move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                use iced::futures::SinkExt;
                let _stop = StopOnDrop(cancel.clone());
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
                let worker_cancel = cancel.clone();
                tokio::task::spawn_blocking(move || {
                    let total = MODEL.bytes();
                    let result = (|| {
                        let mut before = present;
                        let mut last = Instant::now();
                        for file in missing {
                            let mut install = Install::begin(dir.join(file.path), expected(file))
                                .map_err(|e| {
                                tr!("could not save the model: {error}", error = e).to_string()
                            })?;
                            let mut progress = |done: u64, _: Option<u64>| {
                                if last.elapsed() >= Duration::from_millis(150) {
                                    last = Instant::now();
                                    let _ = tx.send(Message::Settings(
                                        SettingsMsg::EmbedDownloadProgress {
                                            done: before + done,
                                            total: Some(total),
                                        },
                                    ));
                                }
                            };
                            match fetch(
                                &MODEL.url(file),
                                file.bytes,
                                &worker_cancel,
                                &mut install,
                                &mut progress,
                            ) {
                                // Cancelled as it ended: not put in place —
                                // the model may be being removed by now.
                                Ok(_) if worker_cancel.load(Ordering::Relaxed) => {
                                    return Err(clew_core::llm::CANCELLED.into());
                                }
                                Ok(_) => {
                                    install.finish()?;
                                }
                                Err(e) => {
                                    return Err(crate::app::voice::download_failure_sized(
                                        &e,
                                        before + install.written(),
                                        MODEL.size_mb(),
                                    ));
                                }
                            }
                            before += file.bytes;
                        }
                        Ok(())
                    })();
                    let _ = tx.send(Message::Settings(SettingsMsg::EmbedDownloaded(result)));
                });
                while let Some(message) = rx.recv().await {
                    let done =
                        matches!(message, Message::Settings(SettingsMsg::EmbedDownloaded(..)));
                    if output.send(message).await.is_err() || done {
                        break;
                    }
                }
            }
        });
        let (task, handle) = Task::run(stream, |m| m).abortable();
        self.local_embedding.download = Some(ModelDownload {
            done: present,
            total: Some(MODEL.bytes()),
            cancel,
            _claim: claim,
            _task: handle.abort_on_drop(),
        });
        self.status = tr!(
            "Downloading {model} ({size} MB)…",
            model = MODEL.name,
            size = MODEL.size_mb()
        );
        task
    }

    /// What the embedding-model messages do.
    pub(crate) fn on_local_embedding(&mut self, msg: SettingsMsg) -> Task<Message> {
        match msg {
            SettingsMsg::EmbedHere(here) => {
                self.settings.embed_here = here;
                Task::none()
            }
            SettingsMsg::DownloadEmbedModel => self.start_embed_download(),
            SettingsMsg::CancelEmbedDownload => {
                if let Some(d) = self.local_embedding.download.take() {
                    d.cancel.store(true, Ordering::Relaxed);
                }
                Task::none()
            }
            SettingsMsg::EmbedDownloadProgress { done, total } => {
                if let Some(d) = self.local_embedding.download.as_mut() {
                    d.done = done;
                    d.total = total;
                }
                Task::none()
            }
            SettingsMsg::EmbedDownloaded(result) => {
                if self.local_embedding.download.take().is_none() {
                    return Task::none();
                }
                match result {
                    Ok(()) => {
                        self.local_embedding.error = None;
                        self.local_embedding.downloaded_at = Some(std::time::Instant::now());
                        self.status = if local::program().is_some() {
                            tr!(
                                "{model} is ready — Find by meaning now runs on this computer",
                                model = MODEL.name
                            )
                        } else {
                            missing_here()
                        };
                        self.embed_model_changed();
                    }
                    Err(e) => {
                        self.status = tr!(
                            "Could not download {model}: {error}",
                            model = MODEL.name,
                            error = e
                        );
                        self.local_embedding.error = Some(e);
                    }
                }
                Task::none()
            }
            SettingsMsg::RemoveEmbedModel => {
                let Some(dir) = local::dir() else {
                    return Task::none();
                };
                // Claimed as a download is, in every window, for as long as
                // its files go: nothing downloads into the folder meanwhile.
                let claim = match Claim::take(dir, ClaimedFor::Removal) {
                    Ok(claim) => claim,
                    Err(held) => {
                        self.status = held.busy(MODEL.name);
                        return Task::none();
                    }
                };
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let _claim = claim;
                            local::remove().map_err(|e| e.to_string())
                        })
                        .await
                        .unwrap_or_else(|_| Err(tr!("removing failed unexpectedly").into()))
                    },
                    |result| Message::Settings(SettingsMsg::EmbedModelRemoved(result)),
                )
            }
            SettingsMsg::EmbedModelRemoved(result) => {
                self.status = match result {
                    Ok(()) => tr!("Removed {model}", model = MODEL.name),
                    Err(e) => tr!(
                        "Could not remove {model}: {error}",
                        model = MODEL.name,
                        error = e
                    ),
                };
                self.embed_model_changed();
                Task::none()
            }
            _ => Task::none(),
        }
    }
}
