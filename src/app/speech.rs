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
//! Which answers are read is the reader's to say
//! ([`ReadAloud`](crate::app::voice::ReadAloud)): those to questions said
//! aloud, every one, or none.
//!
//! It stops at Esc, at the voice key — which then listens, so the reader
//! can speak over Clew as over a person — at a new question, and at Stop,
//! Clear or closing Ask. With the microphone on, the reader can also just
//! talk over Clew ([`VoicePrefs::talk_over`]): the microphone stays open
//! while Clew speaks, Clew's own voice taken out of what it hears
//! ([`clew_voice::echo`]); Clew pauses at the first words said over it, and
//! stops once they are heard to be words — not its own, heard back — or
//! goes on where it paused when they were not. With that off, the
//! microphone waits while Clew reads an answer aloud, and opens again once
//! all is said.
//!
//! [`VoicePrefs::talk_over`]: crate::app::voice::VoicePrefs::talk_over
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
use crate::app::voice::{Claim, ClaimedFor, ModelDownload, StopOnDrop};
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
    /// The device's rate, in Hz: what is played is made at it beforehand.
    fn rate(&self) -> u32;
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
    /// Hold what is playing, to go on from there ([`Playing::resume`]).
    fn pause(&self);
    /// Go on playing what was held.
    fn resume(&self);
}

impl Playing for Player {
    fn play(&self, samples: &[f32], rate: u32) {
        Player::play(self, samples, rate);
    }
    fn rate(&self) -> u32 {
        Player::rate(self)
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
    fn pause(&self) {
        Player::pause(self);
    }
    fn resume(&self) {
        Player::resume(self);
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
    /// `voice` at `speed`, given to `part` as it is made — whole, or a part
    /// at a time from a voice that streams: samples, one channel, and their
    /// rate.
    #[allow(clippy::too_many_arguments)]
    fn say(
        &self,
        model: &'static VoiceModel,
        dir: &Path,
        text: &str,
        lang: &str,
        voice: i32,
        speed: f32,
        part: &mut dyn FnMut(Vec<f32>, u32) -> bool,
    ) -> Result<(), SayError>;
    /// Let `model` go, if it is loaded, and do `work` — remove its files —
    /// with nothing said meanwhile: what would be is said once `work` is
    /// done.
    fn stop_while(&self, model: &'static VoiceModel, work: &mut dyn FnMut());
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
        part: &mut dyn FnMut(Vec<f32>, u32) -> bool,
    ) -> Result<(), SayError> {
        match &self.speakers {
            Some(speakers) => speakers.say(model, dir, text, lang, voice, speed, part),
            None => Err(SayError::Voice(
                tr!("clew-speak, which reads answers aloud, is not installed next to Clew").into(),
            )),
        }
    }
    fn stop_while(&self, model: &'static VoiceModel, work: &mut dyn FnMut()) {
        match &self.speakers {
            Some(speakers) => speakers.stop_while(model, work),
            None => work(),
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

/// A sentence said, or a part of it: its samples (one channel) and their
/// rate. Shown in a message's debug form by its length alone.
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
    hushed: HashSet<u64>,
    /// Sentences waiting to be made into speech, in order.
    queue: VecDeque<Sentence>,
    /// A sentence is being made.
    making: bool,
    /// Bumped by every stop: what was being made before it is dropped when
    /// it comes.
    round: u64,
    /// [`Self::round`], as the sentence being made off the UI thread reads
    /// it: a sentence made for a round gone stops at its next part, and the
    /// voice with it — what was asked next waited for it to be made whole.
    current: Arc<std::sync::atomic::AtomicU64>,
    /// The output, while Clew speaks.
    output: Option<Box<dyn Playing>>,
    /// Since when nothing has been left to play, nor to make.
    quiet_since: Option<Instant>,
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
    /// The answer last brought on screen because it would not be heard
    /// ([`App::show_unheard`]): once an answer, so a card put away stays
    /// away.
    shown: Option<u64>,
    /// The models what is being said was made with.
    made_with: HashSet<&'static str>,
    /// Voice models downloading, by id.
    pub downloads: HashMap<&'static str, ModelDownload>,
    /// The last download's failure, by model id.
    pub download_errors: HashMap<&'static str, String>,
    pub(crate) open_output: OpenOutput,
    pub(crate) synthesizer: Arc<dyn Synthesizer>,
    /// Since when Clew is held while the reader talks over it — until what
    /// they said is heard, and told from Clew's own voice
    /// ([`said_over`](crate::app::voice::said_over)).
    barged: Option<Instant>,
    /// The last sentences made to be said: what the microphone may still
    /// hear of Clew's own voice, its echo not all taken out.
    said_lately: VecDeque<String>,
}

/// How many of the sentences last made [`Speech::said_lately`] keeps.
const SAID_LATELY: usize = 6;

/// How long Clew is held for words said over it at most, before it goes
/// on: what was said is heard by then.
const HELD_AT_MOST: Duration = Duration::from_secs(12);

impl Speech {
    pub(crate) fn new() -> Self {
        Self {
            reading: None,
            hushed: HashSet::new(),
            queue: VecDeque::new(),
            making: false,
            round: 0,
            current: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            output: None,
            quiet_since: None,
            level: 0.0,
            progress: None,
            reopened: false,
            stall: (STALLED_AFTER_ERROR, STALLED),
            told: HashSet::new(),
            shown: None,
            made_with: HashSet::new(),
            downloads: HashMap::new(),
            download_errors: HashMap::new(),
            open_output: Arc::new(open_system_output),
            synthesizer: SYSTEM.clone(),
            barged: None,
            said_lately: VecDeque::new(),
        }
    }

    /// Whether Clew is held while the reader talks over it.
    pub(crate) fn held(&self) -> bool {
        self.barged.is_some()
    }

    /// The reader talks over Clew: what is being said is held — faded out,
    /// kept to go on from — while what they say is heard. Nothing playing,
    /// nothing is held.
    pub(crate) fn hold(&mut self) {
        if self.barged.is_none()
            && let Some(output) = &self.output
        {
            output.pause();
            self.barged = Some(Instant::now());
        }
    }

    /// Go on with what was held, where it was.
    pub(crate) fn go_on(&mut self) {
        if self.barged.take().is_some()
            && let Some(output) = &self.output
        {
            output.resume();
            // The pause was the reader's, not the output's: a stall is
            // counted from now.
            self.progress = None;
        }
    }

    /// Held longer than words said over it take to be heard.
    pub(crate) fn held_too_long(&self) -> bool {
        self.barged
            .is_some_and(|since| since.elapsed() >= HELD_AT_MOST)
    }

    /// Held as if since `ago`.
    #[cfg(test)]
    pub(crate) fn hold_since_for_test(&mut self, ago: Duration) {
        if self.barged.is_some() {
            self.barged = Instant::now().checked_sub(ago);
        }
    }

    /// The sentences last made to be said.
    pub(crate) fn said_lately(&self) -> Vec<String> {
        self.said_lately.iter().cloned().collect()
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
            self.hushed.insert(stream);
        }
        self.queue.clear();
        self.making = false;
        self.round += 1;
        self.current
            .store(self.round, std::sync::atomic::Ordering::Relaxed);
        if let Some(output) = self.output.take() {
            close_later(output);
        }
        self.quiet_since = None;
        self.level = 0.0;
        self.progress = None;
        self.reopened = false;
        self.made_with.clear();
        self.barged = None;
    }
}

/// What Settings' Try says with `model`: a sentence in the interface's
/// language when the model speaks it, else in one it does.
fn sample(model: &VoiceModel) -> Sentence {
    // Its own words in a language Supertonic speaks here, which none of the
    // interface's are.
    const DUTCH: &str = "Zo klinkt Clew wanneer het een antwoord voorleest.";
    let ui = crate::i18n::current().code();
    if model.speaks(ui) {
        Sentence {
            text: tr!("This is how Clew sounds when it reads an answer aloud.").to_string(),
            lang: clew_voice::say::known_language(ui).unwrap_or("en"),
        }
    } else if model.speaks("en") {
        Sentence {
            text: "This is how Clew sounds when it reads an answer aloud.".into(),
            lang: "en",
        }
    } else {
        Sentence {
            text: DUTCH.into(),
            lang: "nl",
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
                last,
            } => self.on_spoken(round, result, opened, last),
            VoiceMsg::SpeakTick => self.on_speak_tick(),
            VoiceMsg::StopSpeaking => {
                if self.stop_speaking() {
                    self.status = tr!("Stopped speaking").into();
                }
                Task::none()
            }
            VoiceMsg::SetSpeak(read_aloud) => {
                self.voice.prefs.read_aloud = read_aloud;
                // What is being read stops when such answers are read no
                // more; with Always, what was not read is not begun midway.
                let reads = self
                    .proj
                    .ask_turns
                    .last()
                    .is_some_and(|t| read_aloud.reads(t.spoken));
                if !reads {
                    self.stop_speaking();
                }
                self.save_voice_pref(crate::app::voice::Pref::Speak);
                Task::none()
            }
            VoiceMsg::SetSpeed(speed) => {
                if (0.5..=2.0).contains(&speed) {
                    self.voice.prefs.speed = speed;
                    self.save_voice_pref(crate::app::voice::Pref::Speed);
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
                self.save_voice_pref(crate::app::voice::Pref::Voice(found.id));
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
                // Claimed as a download is, in every window, for as long as
                // its files go: nothing downloads into the folder, and
                // nothing is said with it — sentences waiting for it are
                // passed over.
                let claim = match Claim::take(speech::dir(&store, model), ClaimedFor::Removal) {
                    Ok(claim) => claim,
                    Err(held) => {
                        self.status = held.busy(crate::i18n::lookup(model.name));
                        return Task::none();
                    }
                };
                // What is being said with it ends — what another model says
                // goes on — and so does the hold on its files, off the UI
                // thread: a sentence being made is waited for, and none is
                // made until the files are gone.
                if self.voice.speech.made_with.contains(model.id) {
                    self.stop_speaking();
                }
                let synthesizer = self.voice.speech.synthesizer.clone();
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let _claim = claim;
                            let mut removed = Ok(());
                            synthesizer.stop_while(model, &mut || {
                                removed = speech::remove(&store, model);
                            });
                            removed.map_err(|e| e.to_string())
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
    /// is ready to be said is said, when such answers are read aloud
    /// ([`ReadAloud`](crate::app::voice::ReadAloud)).
    pub(crate) fn read_answer(&mut self, stream: u64, done: bool) -> Task<Message> {
        if self.voice.speech.hushed.contains(&stream) {
            return Task::none();
        }
        let read_aloud = self.voice.prefs.read_aloud;
        // The newest answer only: one an earlier question gets late — its
        // close arriving after the next was asked — is not said over it.
        let Some(spoken) = self
            .proj
            .ask_turns
            .last()
            .filter(|t| t.stream == stream)
            .map(|t| t.spoken)
        else {
            return Task::none();
        };
        if !read_aloud.reads(spoken) {
            if spoken {
                self.show_unheard(stream);
            }
            return Task::none();
        }
        let Some(turn) = self.proj.ask_turns.last() else {
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
        // Read aloud: the microphone, on, waits for it.
        self.hold_mic_for_clew();
        self.speak_next()
    }

    /// The answer on `stream`, being read, cut back to its first `from`
    /// bytes — a step's remark before its tool calls taken back out
    /// (`AskMsg::DraftEnd`): what was ready of the remark is said through,
    /// as a person finishes what they were saying, and reading goes on from
    /// where the answer now ends. A step asked `again` writes the same text
    /// anew: reading keeps its place, and goes on in the new text from the
    /// sentence the first try had come to — it was said through, then said
    /// again from its start.
    pub(crate) fn answer_cut_back(
        &mut self,
        stream: u64,
        from: usize,
        again: bool,
    ) -> Task<Message> {
        if again {
            return Task::none();
        }
        let Some(turn) = self.proj.ask_turns.iter().find(|t| t.stream == stream) else {
            return Task::none();
        };
        let kept = turn.answer_md.get(..from).unwrap_or_default();
        let speech = &mut self.voice.speech;
        let Some((_, reader)) = speech.reading.as_mut().filter(|(s, _)| *s == stream) else {
            return Task::none();
        };
        let rest = reader.next(&turn.answer_md, true);
        reader.cut_back(kept);
        speech.queue.extend(rest);
        self.speak_next()
    }

    /// Stop speaking — what is said, made, or waits to be — and stop reading
    /// the answer being read, or about to be: the one still coming. With
    /// the microphone on, it opens. Whether anything was being said.
    /// The answer on `stream` will not be heard — not read aloud, or no
    /// voice here to read it — so it is shown: a question said is sent
    /// without the card, and its answer would otherwise be nowhere to be
    /// seen. Once an answer, so the card put away stays away.
    fn show_unheard(&mut self, stream: u64) {
        if self.voice.speech.shown != Some(stream) {
            self.voice.speech.shown = Some(stream);
            self.reveal_ask();
        }
    }

    pub(crate) fn stop_speaking(&mut self) -> bool {
        let read_aloud = self.voice.prefs.read_aloud;
        let coming = self
            .proj
            .ask_turns
            .last()
            .filter(|t| t.streaming && read_aloud.reads(t.spoken))
            .map(|t| t.stream);
        let speech = &mut self.voice.speech;
        let was = speech.is_speaking();
        speech.hush();
        if let Some(stream) = coming {
            speech.hushed.insert(stream);
        }
        self.resume_listening();
        was
    }

    /// Whether an answer Clew will read aloud is still coming in: the
    /// microphone waits for it, as it does while Clew speaks.
    pub(crate) fn answer_read_aloud_coming(&self) -> bool {
        let read_aloud = self.voice.prefs.read_aloud;
        self.proj.ask_turns.last().is_some_and(|t| {
            t.streaming
                && read_aloud.reads(t.spoken)
                && !self.voice.speech.hushed.contains(&t.stream)
        })
    }

    /// The answer on `stream` was stopped: what is said of it — and only of
    /// it — stops, and no more of it is said. Another answer being said
    /// goes on: a stopped one can be closed after the next was asked.
    pub(crate) fn hush_answer(&mut self, stream: u64) {
        let speech = &mut self.voice.speech;
        if speech.reading.as_ref().is_some_and(|(s, _)| *s == stream) {
            speech.hush();
        }
        speech.hushed.insert(stream);
    }

    /// Clew could not go on speaking, saying `why` on the status line: with
    /// the microphone on, it opens as it would have once all was said.
    fn give_up_speaking(&mut self, why: String) -> Task<Message> {
        self.voice.speech.hush();
        self.status = why;
        self.resume_listening();
        Task::none()
    }

    /// Say `model`'s sample, as Settings' Try does: whatever was being said
    /// stops.
    fn try_speech(&mut self, model: &'static VoiceModel) -> Task<Message> {
        self.stop_speaking();
        if !self.speech_installed(model) {
            return Task::none();
        }
        self.hold_mic_for_clew();
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
            if self.voice.speech.queue.is_empty() {
                return Task::none();
            }
            return self
                .give_up_speaking(tr!("There is no data directory to keep the voice in").into());
        };
        // An answer's sentence no voice here can say: the answer is shown
        // instead. Not a sample of a voice in Settings — that is not an
        // answer.
        let unheard = |app: &mut Self| {
            if only.is_none()
                && let Some(stream) = app.proj.ask_turns.last().map(|t| t.stream)
            {
                app.show_unheard(stream);
            }
        };
        while let Some(sentence) = self.voice.speech.queue.pop_front() {
            let Some(model) = only.or_else(|| speech::for_language(sentence.lang)) else {
                unheard(self);
                continue;
            };
            // Being downloaded or removed, in any window: as good as not here.
            let changing = crate::app::voice::downloading(&speech::dir(&store, model));
            if changing {
                unheard(self);
                continue;
            }
            if !speech::installed(&store, model) {
                unheard(self);
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
            let lately = &mut self.voice.speech.said_lately;
            lately.push_back(sentence.text.clone());
            while lately.len() > SAID_LATELY {
                lately.pop_front();
            }
            let voice = self.voice.prefs.voice_for(model);
            let speed = self.voice.prefs.speed;
            let synthesizer = self.voice.speech.synthesizer.clone();
            // No output yet: it opens while the sentence is made. Either way
            // the sentence is made at the device's rate here, off the UI
            // thread, and only queued there.
            let open = self
                .voice
                .speech
                .output
                .is_none()
                .then(|| self.voice.speech.open_output.clone());
            let device_rate = self.voice.speech.output.as_ref().map(|o| o.rate());
            self.voice.speech.making = true;
            self.voice.speech.made_with.insert(model.id);
            self.voice.speech.quiet_since = None;
            let round = self.voice.speech.round;
            let current = self.voice.speech.current.clone();
            // Made off the UI thread, each part sent as it comes: a voice
            // that streams is heard while the rest is still being made.
            let stream = iced::stream::channel(
                16,
                move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                    use iced::futures::SinkExt;
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
                    tokio::task::spawn_blocking(move || {
                        let mut opening = open.map(|open| std::thread::spawn(move || open()));
                        let mut device_rate = device_rate;
                        // The output opened, taken with the first part, its rate
                        // what the parts are made at.
                        let mut take_opened = |device_rate: &mut Option<u32>| -> Option<Opened> {
                            let opened: Opening = opening.take()?.join().unwrap_or_else(|_| {
                                Err(tr!("speaking failed unexpectedly").into())
                            });
                            if let Ok(output) = &opened {
                                device_rate.get_or_insert(output.rate());
                            }
                            Some(Opened::new(opened))
                        };
                        let mut resampler: Option<(u32, clew_voice::audio::Resampler)> = None;
                        let said = synthesizer.say(
                            model,
                            &dir,
                            &sentence.text,
                            sentence.lang,
                            voice,
                            speed,
                            &mut |samples, rate| {
                                // Stopped since it began: no more of it.
                                if current.load(std::sync::atomic::Ordering::Relaxed) != round {
                                    return false;
                                }
                                let opened = take_opened(&mut device_rate);
                                let to = device_rate.unwrap_or(rate);
                                // One resampler for all the parts: they join as
                                // if made whole.
                                let (_, resampler) = resampler.get_or_insert_with(|| {
                                    (to, clew_voice::audio::Resampler::new(rate, to))
                                });
                                let audio = Audio {
                                    samples: Arc::new(resampler.push(&samples)),
                                    rate: to,
                                };
                                let _ = tx.send(Message::Voice(VoiceMsg::Spoken {
                                    round,
                                    result: Ok(audio),
                                    opened,
                                    last: false,
                                }));
                                true
                            },
                        );
                        let opened = take_opened(&mut device_rate);
                        let rest = match resampler.as_mut() {
                            Some((to, resampler)) => Audio {
                                samples: Arc::new(resampler.finish()),
                                rate: *to,
                            },
                            None => Audio {
                                samples: Arc::new(Vec::new()),
                                rate: device_rate.unwrap_or(1),
                            },
                        };
                        let _ = tx.send(Message::Voice(VoiceMsg::Spoken {
                            round,
                            result: said.map(|()| rest),
                            opened,
                            last: true,
                        }));
                    });
                    while let Some(message) = rx.recv().await {
                        let last =
                            matches!(message, Message::Voice(VoiceMsg::Spoken { last: true, .. }));
                        if output.send(message).await.is_err() || last {
                            break;
                        }
                    }
                },
            );
            return Task::run(stream, |message| message);
        }
        Task::none()
    }

    /// A sentence made, or a part of it (all but its `last`): played after
    /// what is playing — and, once all of it is made, the next one made
    /// meanwhile.
    fn on_spoken(
        &mut self,
        round: u64,
        result: Result<Audio, SayError>,
        opened: Option<Opened>,
        last: bool,
    ) -> Task<Message> {
        let opened = opened.and_then(|o| o.take());
        if round != self.voice.speech.round {
            if let Some(Ok(output)) = opened {
                close_later(output);
            }
            return Task::none();
        }
        if last {
            self.voice.speech.making = false;
        }
        match opened {
            Some(Ok(output)) => match self.voice.speech.output {
                Some(_) => close_later(output),
                None => self.voice.speech.output = Some(output),
            },
            Some(Err(e)) => {
                return self.give_up_speaking(tr!(
                    "Can't speak: {error}",
                    error = crate::i18n::message(&e)
                ));
            }
            None => {}
        }
        match result {
            Ok(audio) => {
                // Opened with the first sentence; one closed since (none is
                // while a sentence is made) is opened again here — not for
                // a sentence's end with nothing left in it.
                if self.voice.speech.output.is_none() && !audio.samples.is_empty() {
                    match (self.voice.speech.open_output)() {
                        Ok(output) => self.voice.speech.output = Some(output),
                        Err(e) => {
                            return self.give_up_speaking(tr!(
                                "Can't speak: {error}",
                                error = crate::i18n::message(&e)
                            ));
                        }
                    }
                }
                if let Some(output) = &self.voice.speech.output {
                    // Given this after playing all it had, how long it plays
                    // nothing is counted from now: the wait for this
                    // sentence was not the output's. Given it with some
                    // still to play, the count goes on — an output that
                    // stopped is found out however many sentences come.
                    if !audio.samples.is_empty() {
                        if output.idle() {
                            self.voice.speech.progress = None;
                        }
                        output.play(&audio.samples, audio.rate);
                    }
                }
                self.voice.speech.quiet_since = None;
                if last {
                    self.speak_next()
                } else {
                    Task::none()
                }
            }
            // This sentence could not be said; the next can.
            Err(SayError::Sentence(_)) => self.speak_next(),
            Err(SayError::Voice(e)) => self.give_up_speaking(tr!(
                "Could not speak: {error}",
                error = crate::i18n::message(&e)
            )),
        }
    }

    /// While Clew speaks: the meter; the output closed once all is said;
    /// then, with the microphone on, it opens.
    fn on_speak_tick(&mut self) -> Task<Message> {
        let speech = &mut self.voice.speech;
        let Some(output) = speech.output.as_ref() else {
            return Task::none();
        };
        // Whether it plays what it has: a failure it reports is taken for a
        // stop only once it plays nothing more — a device's hiccup is played
        // through.
        let played = output.played();
        // Held while the reader talks over Clew: it plays nothing on
        // purpose. Held too long — what was said was not heard — it goes on.
        if speech.barged.is_some() {
            if speech.held_too_long() {
                speech.go_on();
            }
            speech.level = 0.0;
            return Task::none();
        }
        match speech.progress {
            // Nothing played since, though there was something to play.
            Some((before, _)) if before == played && !output.idle() => {}
            Some((before, _)) if before != played => {
                speech.progress = Some((played, Instant::now()));
                // Playing again: it may be opened again should it stop.
                speech.reopened = false;
            }
            // Idle — nothing to play is no stall — or just given something.
            _ => speech.progress = Some((played, Instant::now())),
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
        speech.made_with.clear();
        speech.quiet_since = None;
        speech.level = 0.0;
        self.resume_listening();
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
        self.give_up_speaking(tr!(
            "Stopped speaking: {error}",
            error = crate::i18n::message(&why)
        ))
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
        let claim = match Claim::take(speech::dir(&store, model), ClaimedFor::Download) {
            Ok(claim) => claim,
            Err(held) => {
                self.status = held.busy(crate::i18n::lookup(model.name));
                return Task::none();
            }
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
                _claim: claim,
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
