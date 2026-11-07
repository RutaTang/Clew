//! Speech: Clew's answers read aloud, on this computer.
//!
//! An answer to a question that was said rather than typed
//! ([`AskTurn::spoken`](crate::app::model::AskTurn)) is read aloud as it
//! streams in, a sentence at a time ([`clew_voice::say::Reader`]), by a
//! voice model the user downloaded once ([`clew_voice::speech`]) running in
//! `clew-speak` — a program beside Clew's own — and played on the default
//! output ([`clew_voice::playback`]). Nothing said or heard leaves the
//! computer.
//!
//! It stops at Esc, at the voice key — which then listens, so the reader
//! can speak over Clew as over a person — at a new question, and at Stop,
//! Clear or closing Ask. With Keep listening on, the microphone opens again
//! only once Clew has finished speaking: it would hear Clew otherwise.
//!
//! Its messages are [`VoiceMsg`]'s, handled here through
//! [`App::update_speech`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use clew_voice::playback::Player;
use clew_voice::say::{Reader, Sentence};
use clew_voice::speaker::{SayError, Speakers};
use clew_voice::speech::{self, VoiceModel};

use crate::app::prelude::*;
use crate::app::voice::{Claim, ModelDownload, StopOnDrop};
use crate::*;

/// How often the output is looked at while Clew speaks: as often as the
/// microphone is while it listens, for the same meter.
pub(crate) const SPEAK_TICK: Duration = crate::app::voice::LISTEN_TICK;
/// How long the output stays open once all is played: the device's own
/// buffer still holds the last of it.
const TAIL: Duration = Duration::from_millis(350);
/// How long the output stays open waiting for more of an answer still
/// coming, before it is closed (to be opened again for what comes).
const WAIT_FOR_MORE: Duration = Duration::from_secs(5);
/// How far ahead of what is heard sentences are made: enough that the next
/// is always ready, not so much that a long answer is all made at once.
const AHEAD_SECONDS: f32 = 20.0;
/// How long a voice model stays loaded with nothing said: it holds a few
/// hundred megabytes.
const UNLOAD_AFTER: Duration = Duration::from_secs(5 * 60);

/// How long an output that reported a failure may play nothing with
/// something to play before it is taken for stopped: a device reports a
/// hiccup it then plays through, as ALSA does through PulseAudio.
const STALLED_AFTER_ERROR: Duration = Duration::from_secs(2);
/// How long an output may play nothing with something to play, failure or
/// not, before it is taken for stopped.
const STALLED: Duration = Duration::from_secs(10);

/// The speeds offered: 1 is the voice's own pace.
pub(crate) const SPEEDS: &[f32] = &[0.8, 0.9, 1.0, 1.1, 1.25, 1.5];

/// The output while Clew speaks: the system's ([`Player`]), or a test's.
pub(crate) trait Playing: Send {
    /// Play `samples` (one channel, at `rate` Hz) after what is queued.
    fn play(&self, samples: &[f32], rate: u32);
    /// Seconds still to play.
    fn queued_seconds(&self) -> f32;
    /// Samples played since it opened: whether it is playing at all.
    fn played(&self) -> u64;
    /// What is still to be played, taken from it, and its rate.
    fn take_rest(&self) -> (Vec<f32>, u32);
    /// Whether nothing is left to play.
    fn idle(&self) -> bool;
    /// How loud what is playing is, 0 to 1.
    fn level(&self) -> f32;
    /// A failure since it opened (the device went away).
    fn error(&self) -> Option<String>;
}

impl Playing for Player {
    fn play(&self, samples: &[f32], rate: u32) {
        Player::play(self, samples, rate);
    }
    fn queued_seconds(&self) -> f32 {
        Player::queued_seconds(self)
    }
    fn played(&self) -> u64 {
        Player::played(self)
    }
    fn take_rest(&self) -> (Vec<f32>, u32) {
        Player::take_rest(self)
    }
    fn idle(&self) -> bool {
        Player::idle(self)
    }
    fn level(&self) -> f32 {
        Player::level(self)
    }
    fn error(&self) -> Option<String> {
        Player::error(self)
    }
}

/// Opens the output: the system's, or a test's. Called off the UI thread:
/// a device can take a while to open.
pub(crate) type OpenOutput = Arc<dyn Fn() -> Result<Box<dyn Playing>, String> + Send + Sync>;

fn open_system_output() -> Result<Box<dyn Playing>, String> {
    Player::start().map(|p| Box::new(p) as Box<dyn Playing>)
}

/// Close `output` off the UI thread: a device can take a while to close.
fn close_later(output: Box<dyn Playing>) {
    let _ = std::thread::Builder::new()
        .name("clew-speaker-close".into())
        .spawn(move || drop(output));
}

/// An output opened, or why it could not be.
type Opening = Result<Box<dyn Playing>, String>;

/// The output a sentence's task opened, handed over in its message — taken
/// once.
#[derive(Clone)]
pub struct Opened(Arc<std::sync::Mutex<Option<Opening>>>);

impl Opened {
    fn new(opened: Opening) -> Self {
        Opened(Arc::new(std::sync::Mutex::new(Some(opened))))
    }

    fn take(&self) -> Option<Opening> {
        self.0.lock().ok()?.take()
    }
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Opened")
    }
}

/// Text made speech: `clew-speak`, or a test's.
pub(crate) trait Synthesizer: Send + Sync {
    /// `text` said in `lang` by `model` (its files in `dir`), in voice
    /// `voice` at `speed`: samples, one channel, and their rate.
    fn say(
        &self,
        model: &'static VoiceModel,
        dir: &Path,
        text: &str,
        lang: &str,
        voice: i32,
        speed: f32,
    ) -> Result<(Vec<f32>, u32), SayError>;
    /// Let the model go — before its files are removed.
    fn stop(&self);
}

/// `clew-speak`, found beside Clew — or not there.
struct System {
    speakers: Option<Speakers>,
}

impl Synthesizer for System {
    fn say(
        &self,
        model: &'static VoiceModel,
        dir: &Path,
        text: &str,
        lang: &str,
        voice: i32,
        speed: f32,
    ) -> Result<(Vec<f32>, u32), SayError> {
        match &self.speakers {
            Some(speakers) => speakers.say(model, dir, text, lang, voice, speed),
            None => Err(SayError::Voice(
                tr!("clew-speak, which speaks for Clew, is not installed beside it").into(),
            )),
        }
    }
    fn stop(&self) {
        if let Some(speakers) = &self.speakers {
            speakers.stop();
        }
    }
}

/// The one `clew-speak` of the process, however many windows speak: a
/// model is loaded once. It is let go after [`UNLOAD_AFTER`] of quiet.
static SYSTEM: LazyLock<Arc<System>> = LazyLock::new(|| {
    let system = Arc::new(System {
        speakers: speak_bin_path().map(Speakers::new),
    });
    if system.speakers.is_some() {
        let idle = system.clone();
        let _ = std::thread::Builder::new()
            .name("clew-speak-idle".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(30));
                    if let Some(speakers) = &idle.speakers {
                        speakers.stop_if_idle(UNLOAD_AFTER);
                    }
                }
            });
    }
    system
});

/// `clew-speak`: beside Clew's own program (as the app bundle and the
/// archives ship it, and as Cargo builds it), else on `PATH` — by absolute
/// path only, never a relative entry.
fn speak_bin_path() -> Option<PathBuf> {
    const NAME: &str = "clew-speak";
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(format!("{NAME}{}", std::env::consts::EXE_SUFFIX));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    clew_core::lsp::store::find_on_path(NAME)
}

/// A sentence said: its samples (one channel) and their rate. Shown in a
/// message's debug form by its length alone.
#[derive(Clone)]
pub struct Audio {
    pub samples: Arc<Vec<f32>>,
    pub rate: u32,
}

impl std::fmt::Debug for Audio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Audio({} samples at {} Hz)",
            self.samples.len(),
            self.rate
        )
    }
}

/// Speech's state in a window.
pub(crate) struct Speech {
    /// The answer being read, by its stream, and how far.
    reading: Option<(u64, Reader)>,
    /// An answer the reader stopped: not read again as more of it comes.
    hushed: Option<u64>,
    /// Sentences waiting to be made into speech, in order.
    queue: VecDeque<Sentence>,
    /// A sentence is being made.
    making: bool,
    /// Bumped by every stop: what was being made before it is dropped when
    /// it comes.
    round: u64,
    /// The output, while Clew speaks.
    output: Option<Box<dyn Playing>>,
    /// Since when nothing has been left to play, nor to make.
    quiet_since: Option<Instant>,
    /// Keep listening: listen once Clew has finished speaking.
    pub listen_after: bool,
    /// How loud what is said is, 0 to 1, for the meter.
    pub level: f32,
    /// The output's last count of samples played, and when it last moved.
    progress: Option<(u64, Instant)>,
    /// The output was opened again after it stopped: once, until it plays.
    reopened: bool,
    /// How long a stalled output is given ([`STALLED_AFTER_ERROR`],
    /// [`STALLED`]) — shorter in tests.
    pub(crate) stall: (Duration, Duration),
    /// The models already said to be missing: said once a window.
    told: HashSet<&'static str>,
    /// Voice models downloading, by id.
    pub downloads: HashMap<&'static str, ModelDownload>,
    /// The last download's failure, by model id.
    pub download_errors: HashMap<&'static str, String>,
    pub(crate) open_output: OpenOutput,
    pub(crate) synthesizer: Arc<dyn Synthesizer>,
}

impl Speech {
    pub(crate) fn new() -> Self {
        Self {
            reading: None,
            hushed: None,
            queue: VecDeque::new(),
            making: false,
            round: 0,
            output: None,
            quiet_since: None,
            listen_after: false,
            level: 0.0,
            progress: None,
            reopened: false,
            stall: (STALLED_AFTER_ERROR, STALLED),
            told: HashSet::new(),
            downloads: HashMap::new(),
            download_errors: HashMap::new(),
            open_output: Arc::new(open_system_output),
            synthesizer: SYSTEM.clone(),
        }
    }

    /// Whether Clew is speaking: something is being said, made, or waits to
    /// be — the output open, between sentences too.
    pub(crate) fn is_speaking(&self) -> bool {
        self.making || !self.queue.is_empty() || self.output.is_some()
    }

    /// Whether what is to be said is being made with nothing yet to hear —
    /// the first sentence of an answer, or the next one after a pause.
    pub(crate) fn preparing(&self) -> bool {
        self.making && self.output.as_ref().is_none_or(|o| o.idle())
    }

    /// Speaking, onto `output`, as loud as `level`: as when a sentence has
    /// just been played.
    #[cfg(test)]
    pub(crate) fn speak_for_test(&mut self, output: Box<dyn Playing>, level: f32) {
        self.output = Some(output);
        self.level = level;
    }

    /// Stop: nothing more is said of what was being said.
    fn hush(&mut self) {
        if let Some((stream, _)) = self.reading.take() {
            self.hushed = Some(stream);
        }
        self.queue.clear();
        self.making = false;
        self.round += 1;
        if let Some(output) = self.output.take() {
            close_later(output);
        }
        self.quiet_since = None;
        self.listen_after = false;
        self.level = 0.0;
        self.progress = None;
        self.reopened = false;
    }
}

/// What Settings' Try says with `model`: a sentence in the interface's
/// language when the model speaks it, else in one it does.
fn sample(model: &VoiceModel) -> Sentence {
    // Its own words in the one language Kokoro speaks here.
    const CHINESE: &str = "这就是 Clew 朗读回答时的声音。";
    let ui = crate::i18n::current().code();
    if model.speaks(ui) {
        let text = match ui {
            "zh" => CHINESE.to_string(),
            _ => tr!("This is how Clew sounds when it reads an answer aloud.").to_string(),
        };
        Sentence {
            text,
            lang: clew_voice::say::known_language(ui).unwrap_or("en"),
        }
    } else if model.speaks("en") {
        Sentence {
            text: "This is how Clew sounds when it reads an answer aloud.".into(),
            lang: "en",
        }
    } else {
        Sentence {
            text: CHINESE.into(),
            lang: "zh",
        }
    }
}

impl App {
    /// Handle one of speech's [`VoiceMsg`]s.
    pub(crate) fn update_speech(&mut self, message: VoiceMsg) -> Task<Message> {
        match message {
            VoiceMsg::Spoken {
                round,
                result,
                opened,
            } => self.on_spoken(round, result, opened),
            VoiceMsg::SpeakTick => self.on_speak_tick(),
            VoiceMsg::StopSpeaking => {
                if self.stop_speaking() {
                    self.status = tr!("Stopped speaking").into();
                }
                Task::none()
            }
            VoiceMsg::SetSpeak(on) => {
                self.voice.prefs.speak = on;
                if !on {
                    self.stop_speaking();
                }
                self.save_voice_prefs();
                Task::none()
            }
            VoiceMsg::SetSpeed(speed) => {
                if (0.5..=2.0).contains(&speed) {
                    self.voice.prefs.speed = speed;
                    self.save_voice_prefs();
                }
                Task::none()
            }
            VoiceMsg::PickSpeechVoice { model, voice } => {
                let Some(found) = speech::find(model) else {
                    return Task::none();
                };
                if !found.voices.iter().any(|v| v.id == voice) {
                    return Task::none();
                }
                self.voice.prefs.voices.insert(model.to_string(), voice);
                self.save_voice_prefs();
                // Heard as it is picked.
                self.try_speech(found)
            }
            VoiceMsg::TrySpeech(model) => match speech::find(model) {
                Some(model) => self.try_speech(model),
                None => Task::none(),
            },
            VoiceMsg::DownloadSpeech(id) => self.start_speech_download(id),
            VoiceMsg::CancelSpeechDownload(id) => {
                if let Some(d) = self.voice.speech.downloads.remove(id) {
                    d.cancel.store(true, Ordering::Relaxed);
                }
                Task::none()
            }
            VoiceMsg::SpeechDownloadProgress { id, done, total } => {
                if let Some(d) = self.voice.speech.downloads.get_mut(id) {
                    d.done = done;
                    d.total = total;
                }
                Task::none()
            }
            VoiceMsg::SpeechDownloaded { id, result } => {
                if self.voice.speech.downloads.remove(id).is_none() {
                    return Task::none();
                }
                let name = crate::i18n::lookup(speech::find(id).map_or(id, |m| m.name));
                match result {
                    Ok(()) => {
                        self.voice.speech.download_errors.remove(id);
                        self.voice.speech.told.remove(id);
                        self.status = tr!(
                            "{model} is ready — answers to what you ask by voice are read aloud",
                            model = name
                        );
                    }
                    Err(e) => {
                        self.status = tr!(
                            "Could not download {model}: {error}",
                            model = name,
                            error = e
                        );
                        self.voice.speech.download_errors.insert(id, e);
                    }
                }
                Task::none()
            }
            VoiceMsg::RemoveSpeech(id) => {
                let (Some(model), Some(store)) = (speech::find(id), self.voice.store.clone())
                else {
                    return Task::none();
                };
                // What is being said with it ends, and so does its hold on
                // the files — off the UI thread: a sentence being made is
                // waited for.
                self.stop_speaking();
                let synthesizer = self.voice.speech.synthesizer.clone();
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            synthesizer.stop();
                            speech::remove(&store, model).map_err(|e| e.to_string())
                        })
                        .await
                        .unwrap_or_else(|_| Err(tr!("removing failed unexpectedly").into()))
                    },
                    move |result| Message::Voice(VoiceMsg::SpeechRemoved { id, result }),
                )
            }
            VoiceMsg::SpeechRemoved { id, result } => {
                let name = crate::i18n::lookup(speech::find(id).map_or(id, |m| m.name));
                self.status = match result {
                    Ok(()) => tr!("Removed {model}", model = name),
                    Err(e) => tr!(
                        "Could not remove {model}: {error}",
                        model = name,
                        error = crate::i18n::message(&e)
                    ),
                };
                Task::none()
            }
            _ => Task::none(),
        }
    }

    /// Whether `model` is downloaded.
    pub(crate) fn speech_installed(&self, model: &VoiceModel) -> bool {
        self.voice
            .store
            .as_ref()
            .is_some_and(|s| speech::installed(s, model))
    }

    /// The answer on stream `stream` grew — or, `done`, is over: what of it
    /// is ready to be said is said, when it answers a question that was
    /// said and answers are read aloud.
    pub(crate) fn read_answer(&mut self, stream: u64, done: bool) -> Task<Message> {
        if !self.voice.prefs.speak || self.voice.speech.hushed == Some(stream) {
            return Task::none();
        }
        let Some(turn) = self
            .proj
            .ask_turns
            .iter()
            .find(|t| t.stream == stream && t.spoken)
        else {
            return Task::none();
        };
        let usual = self
            .voice
            .prefs
            .language
            .clone()
            .unwrap_or_else(|| crate::i18n::current().code().to_string());
        let speech = &mut self.voice.speech;
        if speech.reading.as_ref().is_none_or(|(s, _)| *s != stream) {
            // Another answer: what was being said of the last one ends.
            if speech.reading.is_some() {
                speech.hush();
            }
            speech.reading = Some((stream, Reader::new(Some(&usual))));
        }
        let Some((_, reader)) = speech.reading.as_mut() else {
            return Task::none();
        };
        let fresh = reader.next(&turn.answer_md, done);
        speech.queue.extend(fresh);
        if done {
            speech.reading = None;
        }
        self.speak_next()
    }

    /// Stop speaking — what is said, made, or waits to be — and stop reading
    /// the answer being read, or about to be: the one still coming. Whether
    /// anything was being said.
    pub(crate) fn stop_speaking(&mut self) -> bool {
        let coming = self
            .proj
            .ask_turns
            .last()
            .filter(|t| t.streaming && t.spoken)
            .map(|t| t.stream);
        let speech = &mut self.voice.speech;
        let was = speech.is_speaking();
        speech.hush();
        if coming.is_some() {
            speech.hushed = coming;
        }
        was
    }

    /// Say `model`'s sample, as Settings' Try does: whatever was being said
    /// stops.
    fn try_speech(&mut self, model: &'static VoiceModel) -> Task<Message> {
        self.stop_speaking();
        if !self.speech_installed(model) {
            return Task::none();
        }
        let sentence = sample(model);
        let speech = &mut self.voice.speech;
        speech.queue.push_back(sentence);
        self.speak_next_with(Some(model))
    }

    /// Make the next sentence into speech, if none is being made and not too
    /// much is waiting to be heard.
    fn speak_next(&mut self) -> Task<Message> {
        self.speak_next_with(None)
    }

    fn speak_next_with(&mut self, only: Option<&'static VoiceModel>) -> Task<Message> {
        let ahead = self
            .voice
            .speech
            .output
            .as_ref()
            .map_or(0.0, |o| o.queued_seconds());
        if self.voice.speech.making || ahead > AHEAD_SECONDS {
            return Task::none();
        }
        let Some(store) = self.voice.store.clone() else {
            if !self.voice.speech.queue.is_empty() {
                self.voice.speech.hush();
                self.status = tr!("There is no data directory to keep the voice in").into();
            }
            return Task::none();
        };
        while let Some(sentence) = self.voice.speech.queue.pop_front() {
            let Some(model) = only.or_else(|| speech::for_language(sentence.lang)) else {
                continue;
            };
            if !speech::installed(&store, model) {
                // Said once a window — the status line is not a nag — and
                // not at all while it downloads.
                let speech = &mut self.voice.speech;
                if !speech.downloads.contains_key(model.id) && speech.told.insert(model.id) {
                    self.status = tr!(
                        "To hear answers read aloud, download {model} ({size} MB) in \
                         Settings → Voice",
                        model = crate::i18n::lookup(model.name),
                        size = model.size_mb()
                    );
                }
                continue;
            }
            let dir = speech::dir(&store, model);
            let voice = self.voice.prefs.voice_for(model);
            let speed = self.voice.prefs.speed;
            let synthesizer = self.voice.speech.synthesizer.clone();
            // No output yet: it opens while the sentence is made.
            let open = self
                .voice
                .speech
                .output
                .is_none()
                .then(|| self.voice.speech.open_output.clone());
            self.voice.speech.making = true;
            self.voice.speech.quiet_since = None;
            let round = self.voice.speech.round;
            return Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        let opening = open.map(|open| std::thread::spawn(move || open()));
                        let result = synthesizer
                            .say(model, &dir, &sentence.text, sentence.lang, voice, speed)
                            .map(|(samples, rate)| Audio {
                                samples: Arc::new(samples),
                                rate,
                            });
                        let opened = opening.map(|opening| {
                            Opened::new(opening.join().unwrap_or_else(|_| {
                                Err(tr!("speaking failed unexpectedly").into())
                            }))
                        });
                        (result, opened)
                    })
                    .await
                    .unwrap_or_else(|_| {
                        (
                            Err(SayError::Voice(tr!("speaking failed unexpectedly").into())),
                            None,
                        )
                    })
                },
                move |(result, opened)| {
                    Message::Voice(VoiceMsg::Spoken {
                        round,
                        result,
                        opened,
                    })
                },
            );
        }
        Task::none()
    }

    /// A sentence made: played after what is playing, and the next one
    /// made meanwhile.
    fn on_spoken(
        &mut self,
        round: u64,
        result: Result<Audio, SayError>,
        opened: Option<Opened>,
    ) -> Task<Message> {
        let opened = opened.and_then(|o| o.take());
        if round != self.voice.speech.round {
            if let Some(Ok(output)) = opened {
                close_later(output);
            }
            return Task::none();
        }
        self.voice.speech.making = false;
        match opened {
            Some(Ok(output)) => match self.voice.speech.output {
                Some(_) => close_later(output),
                None => self.voice.speech.output = Some(output),
            },
            Some(Err(e)) => {
                self.voice.speech.hush();
                self.status = tr!("Can't speak: {error}", error = crate::i18n::message(&e));
                return Task::none();
            }
            None => {}
        }
        match result {
            Ok(audio) => {
                // Opened with the first sentence; one closed since (none is
                // while a sentence is made) is opened again here.
                if self.voice.speech.output.is_none() {
                    match (self.voice.speech.open_output)() {
                        Ok(output) => self.voice.speech.output = Some(output),
                        Err(e) => {
                            self.voice.speech.hush();
                            self.status =
                                tr!("Can't speak: {error}", error = crate::i18n::message(&e));
                            return Task::none();
                        }
                    }
                }
                if let Some(output) = &self.voice.speech.output {
                    output.play(&audio.samples, audio.rate);
                }
                self.voice.speech.quiet_since = None;
                self.speak_next()
            }
            // This sentence could not be said; the next can.
            Err(SayError::Sentence(_)) => self.speak_next(),
            Err(SayError::Voice(e)) => {
                self.voice.speech.hush();
                self.status = tr!("Could not speak: {error}", error = crate::i18n::message(&e));
                Task::none()
            }
        }
    }

    /// While Clew speaks: the meter; the output closed once all is said;
    /// then, with Keep listening on, the microphone opened.
    fn on_speak_tick(&mut self) -> Task<Message> {
        let speech = &mut self.voice.speech;
        let Some(output) = speech.output.as_ref() else {
            return Task::none();
        };
        // Whether it plays what it has: a failure it reports is taken for a
        // stop only once it plays nothing more — a device's hiccup is played
        // through.
        let played = output.played();
        match speech.progress {
            Some((before, _)) if before == played => {}
            Some(_) => {
                speech.progress = Some((played, Instant::now()));
                // Playing again: it may be opened again should it stop.
                speech.reopened = false;
            }
            None => speech.progress = Some((played, Instant::now())),
        }
        let stalled = match speech.progress {
            Some((_, since)) if !output.idle() => since.elapsed(),
            _ => Duration::ZERO,
        };
        let error = output.error();
        let (after_error, at_all) = speech.stall;
        if (error.is_some() && stalled >= after_error) || stalled >= at_all {
            return self.on_output_stopped(error);
        }
        speech.level = output.level();
        let quiet = output.idle() && !speech.making && speech.queue.is_empty();
        if !quiet {
            speech.quiet_since = None;
            return self.speak_next();
        }
        let since = *speech.quiet_since.get_or_insert_with(Instant::now);
        let wait = if speech.reading.is_some() {
            WAIT_FOR_MORE
        } else {
            TAIL
        };
        if since.elapsed() < wait {
            return Task::none();
        }
        // All said.
        if let Some(output) = speech.output.take() {
            close_later(output);
        }
        speech.quiet_since = None;
        speech.level = 0.0;
        if speech.listen_after && speech.reading.is_none() {
            speech.listen_after = false;
            return self.resume_listening();
        }
        Task::none()
    }

    /// The output stopped playing, saying why (`error`) or not: opened
    /// again — the default output may be another device now — with what was
    /// left to play, once; if that one stops too, Clew stops speaking and
    /// says why.
    fn on_output_stopped(&mut self, error: Option<String>) -> Task<Message> {
        let speech = &mut self.voice.speech;
        let why = error.unwrap_or_else(|| tr!("the speaker stopped playing").to_string());
        if !speech.reopened
            && let Some(stopped) = speech.output.take()
        {
            speech.reopened = true;
            speech.progress = None;
            let (rest, rate) = stopped.take_rest();
            close_later(stopped);
            if let Ok(output) = (speech.open_output)() {
                output.play(&rest, rate);
                speech.output = Some(output);
                return self.speak_next();
            }
        }
        speech.hush();
        self.status = tr!(
            "Stopped speaking: {error}",
            error = crate::i18n::message(&why)
        );
        Task::none()
    }

    /// Download voice model `id`, file by file, each verified as it lands.
    fn start_speech_download(&mut self, id: &'static str) -> Task<Message> {
        let Some(model) = speech::find(id) else {
            return Task::none();
        };
        if self.voice.speech.downloads.contains_key(id) {
            return Task::none();
        }
        let Some(store) = self.voice.store.clone() else {
            self.status = tr!("There is no data directory to keep the model in").into();
            return Task::none();
        };
        let Some(claim) = Claim::take(speech::dir(&store, model)) else {
            self.status = tr!(
                "{model} is already downloading in another window",
                model = crate::i18n::lookup(model.name)
            );
            return Task::none();
        };
        self.voice.speech.download_errors.remove(id);
        let present = speech::present_bytes(&store, model);
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
                    let _claim = claim;
                    let total = model.bytes();
                    let result = (|| {
                        let mut before = present;
                        let mut last = Instant::now();
                        for file in speech::missing(&store, model) {
                            let mut install = speech::begin(&store, model, file).map_err(|e| {
                                tr!("could not save the model: {error}", error = e).to_string()
                            })?;
                            let mut progress = |done: u64, _: Option<u64>| {
                                if last.elapsed() >= Duration::from_millis(150) {
                                    last = Instant::now();
                                    let _ =
                                        tx.send(Message::Voice(VoiceMsg::SpeechDownloadProgress {
                                            id,
                                            done: before + done,
                                            total: Some(total),
                                        }));
                                }
                            };
                            match fetch(
                                &model.url(file),
                                file.bytes,
                                &worker_cancel,
                                &mut install,
                                &mut progress,
                            ) {
                                Ok(_) => {
                                    install.finish()?;
                                }
                                Err(e) => {
                                    return Err(crate::app::voice::download_failure_sized(
                                        &e,
                                        before + install.written(),
                                        model.size_mb(),
                                    ));
                                }
                            }
                            before += file.bytes;
                        }
                        Ok(())
                    })();
                    let _ = tx.send(Message::Voice(VoiceMsg::SpeechDownloaded { id, result }));
                });
                while let Some(message) = rx.recv().await {
                    let done = matches!(message, Message::Voice(VoiceMsg::SpeechDownloaded { .. }));
                    if output.send(message).await.is_err() || done {
                        break;
                    }
                }
            }
        });
        let (task, handle) = Task::run(stream, |m| m).abortable();
        self.voice.speech.downloads.insert(
            id,
            ModelDownload {
                done: present,
                total: Some(model.bytes()),
                cancel,
                _task: handle.abort_on_drop(),
            },
        );
        self.status = tr!(
            "Downloading {model} ({size} MB)…",
            model = crate::i18n::lookup(model.name),
            size = model.size_mb()
        );
        task
    }
}
