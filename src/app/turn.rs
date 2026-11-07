//! When a speaker has finished: Smart Turn ([`clew_voice::turn`]) judges
//! each pause while listening.
//!
//! Without it, listening ends after a fixed pause ([`END_SILENCE`]): long
//! enough not to cut most people off mid-thought, and so a wait after every
//! short command. With it, a short pause ([`JUDGE_AFTER`]) is judged: a
//! turn that sounds finished ends there, and one that does not is waited
//! for up to [`HOLD_FOR`]. Whatever goes wrong with the model — not
//! downloaded, `clew-speak` missing, an answer too slow or none — listening
//! ends as it would without it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use clew_voice::turn::{self as smart, Judges, MODEL};
use iced::Task;

use crate::app::voice::{Claim, ClaimedFor, END_SILENCE, ModelDownload, StopOnDrop, VoiceMsg};
use crate::*;

/// A pause after speech this long is judged.
pub(crate) const JUDGE_AFTER: f32 = 0.3;
/// A turn judged unfinished is waited for this long, before listening
/// ends all the same.
pub(crate) const HOLD_FOR: f32 = 3.0;
/// How sure the model must be that the speaker is done.
pub(crate) const DONE: f32 = 0.5;
/// How long the model stays loaded with nothing asked of it.
pub(crate) const IDLE: Duration = Duration::from_secs(300);

/// What judges a pause: `clew-speak turn` ([`Judges`]), or a test's.
pub(crate) trait Judging: Send + Sync {
    /// Load the model in `file`, so the first pause is judged at once.
    fn warm(&self, file: &Path) -> Result<(), String>;
    /// How likely it is, 0 to 1, that the speaker of `samples` (16 kHz)
    /// has finished.
    fn judge(&self, file: &Path, samples: &[f32]) -> Result<f32, String>;
    /// Let the model go.
    fn stop(&self);
}

impl Judging for Judges {
    fn warm(&self, file: &Path) -> Result<(), String> {
        Judges::warm(self, file)
    }
    fn judge(&self, file: &Path, samples: &[f32]) -> Result<f32, String> {
        Judges::judge(self, file, samples)
    }
    fn stop(&self) {
        Judges::stop(self);
    }
}

/// The one `clew-speak turn` of this process, shared by its windows — if
/// `clew-speak` is beside Clew. The model is let go after [`IDLE`] with
/// nothing asked of it.
static JUDGES: LazyLock<Option<Arc<Judges>>> = LazyLock::new(|| {
    let judges = Arc::new(Judges::new(clew_core::embed::local::program()?));
    let idle = judges.clone();
    let _ = std::thread::Builder::new()
        .name("clew-turn-idle".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(30));
                idle.stop_if_idle(IDLE);
            }
        });
    Some(judges)
});

/// Smart Turn's state in a window.
pub(crate) struct TurnState {
    /// What judges a pause; `None` when `clew-speak` is not beside Clew.
    pub(crate) judges: Option<Arc<dyn Judging>>,
    pub(crate) download: Option<ModelDownload>,
    /// The last download's or removal's failure.
    pub(crate) error: Option<String>,
    /// While listening: the pause being judged.
    pub(crate) pause: Pause,
}

impl TurnState {
    pub(crate) fn new() -> Self {
        Self {
            judges: JUDGES.clone().map(|j| j as Arc<dyn Judging>),
            download: None,
            error: None,
            pause: Pause::default(),
        }
    }
}

/// The pause being judged, while listening.
#[derive(Debug, Default)]
pub(crate) struct Pause {
    /// Which listening this is, and which pause of it: an answer about an
    /// earlier one is dropped.
    pub(crate) listen: u64,
    pub(crate) number: u64,
    /// The quiet last read: less of it now is speech come back — a pause
    /// of its own, judged afresh.
    last_quiet: Option<f32>,
    asked: bool,
    /// What the model said of it — finished or not; `None` until it
    /// answers, and when it could not.
    verdict: Option<bool>,
}

impl App {
    /// The model's file, when it is here and something can run it.
    pub(crate) fn turn_model(&self) -> Option<PathBuf> {
        let store = self.voice.store.as_ref()?;
        self.voice.turn.judges.as_ref()?;
        smart::installed(store).then(|| smart::path(store))
    }

    /// Listening began: its pauses are judged afresh, and the model loads
    /// meanwhile — off this thread — so the first is judged at once.
    pub(crate) fn turn_listening_began(&mut self) {
        let pause = &mut self.voice.turn.pause;
        *pause = Pause {
            listen: pause.listen + 1,
            ..Pause::default()
        };
        if let (Some(file), Some(judges)) = (self.turn_model(), self.voice.turn.judges.clone()) {
            std::thread::Builder::new()
                .name("clew-turn-warm".into())
                .spawn(move || {
                    let _ = judges.warm(&file);
                })
                .ok();
        }
    }

    /// Whether `quiet` — seconds of it since the speaker last spoke, `None`
    /// before they have — ends listening; and the judgement to ask for, if
    /// this pause is long enough to be judged and has not been.
    pub(crate) fn pause_ends(&mut self, quiet: Option<f32>) -> (bool, Task<Message>) {
        let Some(quiet) = quiet else {
            return (false, Task::none());
        };
        let model = self.turn_model();
        let pause = &mut self.voice.turn.pause;
        if pause.last_quiet.is_some_and(|last| quiet < last) {
            pause.number += 1;
            pause.asked = false;
            pause.verdict = None;
        }
        pause.last_quiet = Some(quiet);
        let (Some(file), Some(judges)) = (model, self.voice.turn.judges.clone()) else {
            return (quiet >= END_SILENCE, Task::none());
        };
        let ends = match pause.verdict {
            // Judged finished: listening ended as the answer came
            // ([`App::on_judged`]); a tick that still finds it ends it too.
            Some(true) => true,
            Some(false) => quiet >= HOLD_FOR,
            None => quiet >= END_SILENCE,
        };
        if ends || pause.asked || quiet < JUDGE_AFTER {
            return (ends, Task::none());
        }
        pause.asked = true;
        let (listen, number) = (pause.listen, pause.number);
        let samples = self
            .voice
            .listening_now()
            .map(|l| l.recent(smart::SECONDS, smart::RATE))
            .unwrap_or_default();
        let task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || judges.judge(&file, &samples))
                    .await
                    .unwrap_or_else(|_| Err(format!("{} failed unexpectedly", MODEL.name)))
            },
            move |result| {
                Message::Voice(VoiceMsg::Judged {
                    listen,
                    pause: number,
                    result,
                })
            },
        );
        (false, task)
    }

    /// What the model said of pause `number` of listening `listen`: kept
    /// if it is still the pause, and if it says the speaker is done,
    /// listening ends now.
    pub(crate) fn on_judged(
        &mut self,
        listen: u64,
        number: u64,
        result: Result<f32, String>,
    ) -> Task<Message> {
        // What went wrong is shown under the model in Settings, and the
        // pause is waited out as without it.
        let p = match result {
            Ok(p) => Some(p),
            Err(e) => {
                self.voice.turn.error = Some(e);
                None
            }
        };
        let listening = self.voice.is_listening();
        let pause = &mut self.voice.turn.pause;
        if !listening || pause.listen != listen || pause.number != number {
            return Task::none();
        }
        pause.verdict = p.map(|p| p >= DONE);
        if pause.verdict == Some(true) {
            return self.update(Message::Voice(VoiceMsg::Stop));
        }
        Task::none()
    }

    /// What Smart Turn's settings messages do.
    pub(crate) fn on_turn_settings(&mut self, msg: VoiceMsg) -> Task<Message> {
        match msg {
            VoiceMsg::DownloadTurn => self.start_turn_download(),
            VoiceMsg::CancelTurnDownload => {
                if let Some(d) = self.voice.turn.download.take() {
                    d.cancel.store(true, Ordering::Relaxed);
                }
                Task::none()
            }
            VoiceMsg::TurnDownloadProgress { done, total } => {
                if let Some(d) = self.voice.turn.download.as_mut() {
                    d.done = done;
                    d.total = total;
                }
                Task::none()
            }
            VoiceMsg::TurnDownloaded(result) => {
                if self.voice.turn.download.take().is_none() {
                    return Task::none();
                }
                match result {
                    Ok(()) => {
                        self.voice.turn.error = None;
                        self.status = tr!(
                            "{model} is ready — listening now ends when you have finished",
                            model = MODEL.name
                        );
                    }
                    Err(e) => {
                        self.status = tr!(
                            "Could not download {model}: {error}",
                            model = MODEL.name,
                            error = e
                        );
                        self.voice.turn.error = Some(e);
                    }
                }
                Task::none()
            }
            VoiceMsg::RemoveTurn => self.remove_turn_model(),
            VoiceMsg::TurnRemoved(result) => {
                match result {
                    Ok(()) => {
                        self.voice.turn.error = None;
                        self.status = tr!("{model} removed", model = MODEL.name);
                    }
                    Err(e) => {
                        self.status = tr!(
                            "Could not remove {model}: {error}",
                            model = MODEL.name,
                            error = e
                        );
                        self.voice.turn.error = Some(e);
                    }
                }
                Task::none()
            }
            _ => Task::none(),
        }
    }

    /// Download the model (the user's go-ahead: the button that says its
    /// size), checked against its size and checksum as it lands.
    fn start_turn_download(&mut self) -> Task<Message> {
        if self.voice.turn.download.is_some() {
            return Task::none();
        }
        let Some(store) = self.voice.store.clone() else {
            self.status = tr!("There is no data directory to keep the model in").into();
            return Task::none();
        };
        let claim = match Claim::take(smart::path(&store), ClaimedFor::Download) {
            Ok(claim) => claim,
            Err(held) => {
                self.status = held.busy(MODEL.name);
                return Task::none();
            }
        };
        self.voice.turn.error = None;
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
                    let result = (|| {
                        let mut install = smart::begin(&store).map_err(|e| {
                            tr!("could not save the model: {error}", error = e).to_string()
                        })?;
                        let mut last = Instant::now();
                        let mut progress = |done: u64, total: Option<u64>| {
                            if last.elapsed() >= Duration::from_millis(150) {
                                last = Instant::now();
                                let _ = tx.send(Message::Voice(VoiceMsg::TurnDownloadProgress {
                                    done,
                                    total,
                                }));
                            }
                        };
                        match fetch(
                            &MODEL.url(),
                            MODEL.bytes,
                            &worker_cancel,
                            &mut install,
                            &mut progress,
                        ) {
                            // Cancelled as it ended: not put in place — the
                            // model may be being removed by now.
                            Ok(_) if worker_cancel.load(Ordering::Relaxed) => {
                                Err(clew_core::llm::CANCELLED.to_string())
                            }
                            Ok(_) => install.finish().map(|_| ()),
                            Err(e) => Err(crate::app::voice::download_failure_sized(
                                &e,
                                install.written(),
                                MODEL.size_mb(),
                            )),
                        }
                    })();
                    let _ = tx.send(Message::Voice(VoiceMsg::TurnDownloaded(result)));
                });
                while let Some(message) = rx.recv().await {
                    let done = matches!(message, Message::Voice(VoiceMsg::TurnDownloaded(..)));
                    if output.send(message).await.is_err() || done {
                        break;
                    }
                }
            }
        });
        let (task, handle) = Task::run(stream, |m| m).abortable();
        self.voice.turn.download = Some(ModelDownload {
            done: 0,
            total: Some(MODEL.bytes),
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

    /// Remove the model: what runs it lets go first — Windows will not
    /// delete a file in use — and nothing downloads it meanwhile.
    fn remove_turn_model(&mut self) -> Task<Message> {
        let Some(store) = self.voice.store.clone() else {
            return Task::none();
        };
        let claim = match Claim::take(smart::path(&store), ClaimedFor::Removal) {
            Ok(claim) => claim,
            Err(held) => {
                self.status = held.busy(MODEL.name);
                return Task::none();
            }
        };
        let judges = self.voice.turn.judges.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let _claim = claim;
                    if let Some(judges) = judges {
                        judges.stop();
                    }
                    smart::remove(&store).map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|_| Err(tr!("the removal failed unexpectedly").into()))
            },
            |result| Message::Voice(VoiceMsg::TurnRemoved(result)),
        )
    }
}
