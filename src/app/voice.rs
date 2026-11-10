//! Voice: talking to the window instead of typing to it.
//!
//! The voice key (⇧⌘S, rebindable) or the microphone button turns the
//! microphone on, and it listens until either is pressed again: a pause once
//! something has been said ends that utterance, which is worked out out of
//! sight while the microphone listens on for the next. It waits only while
//! Clew reads an answer aloud — it would hear Clew. What was heard is
//! transcribed on this machine, by whisper.cpp on a model the user
//! downloaded once ([`clew_voice::models`]): nothing that is said leaves the
//! computer.
//!
//! The words are then read as one of the window's commands when they
//! plainly name one ("open the call graph", "hide the minimap" —
//! [`clew_voice::intent`]), which runs as if picked from the command
//! palette; a command that costs something (a paid pass, starting the
//! program) asks first. Anything else is a question, and goes to Ask.
//!
//! Its messages, [`VoiceMsg`], arrive through `App::update_voice`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use clew_voice::capture::{Recorder, Recording};
use clew_voice::models::{self, Model, Store};
use clew_voice::whisper::{Options, Whisper};

use crate::app::prelude::*;
use crate::*;

/// Most of one utterance kept: a minute.
pub(crate) const MAX_SECONDS: u32 = 60;
/// The pause after speech that ends listening on its own.
pub(crate) const END_SILENCE: f32 = 1.2;
/// How long the microphone keeps the quiet before it lets it go and
/// listens afresh: what it keeps is for what is said.
pub(crate) const NO_SPEECH_TIMEOUT: f32 = 8.0;
/// A second press of the voice key sooner than this after the last one is
/// the key repeating while held — or bouncing — not a press: ignored.
pub(crate) const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(400);
/// How often the level meter and the pause are read while listening.
pub(crate) const LISTEN_TICK: Duration = Duration::from_millis(60);
/// Kept of what was heard before speech said over Clew began, in seconds:
/// a soft first sound, before the run that was taken for speech.
const OVER_LEAD: f32 = 0.5;

/// The microphone while it listens: the system's ([`Recorder`]), or a
/// test's.
pub(crate) trait Listening: Send {
    /// How loud the latest moment was, 0 to 1.
    fn level(&self) -> f32;
    /// Seconds of quiet since the speaker last spoke; `None` before any
    /// speech.
    fn silence_after_speech(&self) -> Option<f32>;
    /// Where speech said over Clew began, in seconds from the start: a
    /// stricter test than [`Listening::silence_after_speech`]'s, which what
    /// is left of Clew's own voice may pass ([`Recorder::speech_over`]).
    fn speech_over(&self) -> Option<f32>;
    /// The last `seconds` heard, and the rate they are at: what Smart Turn
    /// judges a pause by ([`crate::app::turn`]).
    fn recent(&self, seconds: f32) -> (Vec<f32>, u32);
    /// Whether it has heard all it may keep.
    fn full(&self) -> bool;
    /// A failure since it started (the device went away).
    fn error(&self) -> Option<String>;
    /// Hand over what was heard so far, and listen on afresh, as opened.
    fn take(&mut self) -> Recording;
    /// Stop, handing over what was heard.
    fn stop(self: Box<Self>) -> Recording;
}

impl Listening for Recorder {
    fn level(&self) -> f32 {
        Recorder::level(self)
    }
    fn silence_after_speech(&self) -> Option<f32> {
        Recorder::silence_after_speech(self)
    }
    fn speech_over(&self) -> Option<f32> {
        Recorder::speech_over(self)
    }
    fn recent(&self, seconds: f32) -> (Vec<f32>, u32) {
        Recorder::recent(self, seconds)
    }
    fn full(&self) -> bool {
        Recorder::full(self)
    }
    fn error(&self) -> Option<String> {
        Recorder::error(self)
    }
    fn take(&mut self) -> Recording {
        Recorder::take(self)
    }
    fn stop(self: Box<Self>) -> Recording {
        Recorder::stop(*self)
    }
}

/// Opens the microphone, keeping at most so many seconds, and taking out
/// of what it hears what Clew says when asked: the system's, or a test's.
pub(crate) type OpenMic = fn(u32, bool) -> Result<Box<dyn Listening>, String>;

fn open_system_mic(max_seconds: u32, cancel_echo: bool) -> Result<Box<dyn Listening>, String> {
    Recorder::start(max_seconds, cancel_echo).map(|r| Box::new(r) as Box<dyn Listening>)
}

/// Speech to text: whisper on a model file, or a test's.
pub(crate) trait Transcriber: Send + Sync {
    /// The words in `audio` (16 kHz, one channel), heard by the model at
    /// `model`; an error that is `"cancelled"` once `cancel` is set.
    fn transcribe(
        &self,
        model: &Path,
        audio: &[f32],
        options: &Options,
        cancel: &AtomicBool,
    ) -> Result<String, String>;
}

/// whisper, with the model last used kept loaded — one for the whole
/// process, however many windows listen: a large model is a gigabyte.
struct Engine {
    loaded: Mutex<Option<Whisper>>,
}

impl Transcriber for Engine {
    fn transcribe(
        &self,
        model: &Path,
        audio: &[f32],
        options: &Options,
        cancel: &AtomicBool,
    ) -> Result<String, String> {
        let mut loaded = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
        if loaded.as_ref().map(Whisper::path) != Some(model) {
            // The old one goes first: two large models need not fit at once.
            *loaded = None;
            *loaded = Some(Whisper::load(model)?);
        }
        match loaded.as_ref() {
            Some(whisper) => whisper.transcribe(audio, options, cancel),
            None => Err(tr!("the speech model did not load").into()),
        }
    }
}

static ENGINE: LazyLock<Arc<Engine>> = LazyLock::new(|| {
    Arc::new(Engine {
        loaded: Mutex::new(None),
    })
});

/// Fetches `url` into `sink` within `max_bytes`, reporting `(done, total)`
/// as it goes; stops once `cancel` is set. Clew's own transport, or a
/// test's.
pub(crate) type Fetch = fn(
    url: &str,
    max_bytes: u64,
    cancel: &AtomicBool,
    sink: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String>;

fn fetch_over_net(
    url: &str,
    max_bytes: u64,
    cancel: &AtomicBool,
    sink: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    if !url.starts_with("https://") {
        return Err(tr!("refusing a model download that is not HTTPS").into());
    }
    // A gigabyte over a link that drops now and then: it picks up where it
    // stopped rather than start again.
    clew_core::net::download_resuming(
        url,
        &[],
        clew_core::net::Limits {
            max_bytes,
            // Slow links are fine (every read has its own stall timeout);
            // this only ends a transfer that trickles forever.
            deadline: Duration::from_secs(3 * 60 * 60),
        },
        cancel,
        &mut WriteRef(sink),
        progress,
    )
}

/// A `&mut dyn Write` as an `impl Write`.
struct WriteRef<'a>(&'a mut dyn std::io::Write);

impl std::io::Write for WriteRef<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// Whether the microphone is open.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum VoicePhase {
    Idle,
    /// The microphone is open.
    Listening {
        since: Instant,
        /// The meter's latest reading, 0 to 1.
        level: f32,
    },
}

/// The latest utterance, and what came of it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Heard {
    /// The words, as whisper wrote them.
    pub text: String,
    pub outcome: HeardOutcome,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HeardOutcome {
    /// The command of this title ran.
    Ran(String),
    /// The command of this title waits for a yes ([`VoiceState::confirm`]).
    Confirm(String),
    /// The words went to Ask.
    Asked,
    /// It failed, saying why.
    Failed(String),
}

/// What was heard and waits for a yes before it is done.
#[derive(Debug, Clone)]
pub(crate) enum Waiting {
    /// A palette command, by its title.
    Command(String),
    /// Something on screen ([`crate::ui::heard`]): as it is shown, and
    /// what its press sends.
    Press {
        shown: String,
        message: Box<Message>,
    },
}

impl Waiting {
    /// What it is called, in the reader's language.
    pub(crate) fn said(&self) -> &str {
        match self {
            Waiting::Command(title) => crate::i18n::lookup(title),
            Waiting::Press { shown, .. } => shown,
        }
    }

    /// What the pill says was done, once it is: a command's title, which
    /// the pill shows translated, or what was pressed as it was shown.
    fn done(&self) -> String {
        match self {
            Waiting::Command(title) => title.clone(),
            Waiting::Press { shown, .. } => shown.clone(),
        }
    }
}

impl App {
    /// Do what waited for a yes.
    fn run_waiting(&mut self, waiting: Waiting) -> Task<Message> {
        match waiting {
            Waiting::Command(title) => self.run_palette(&title),
            Waiting::Press { message, .. } => self.update(*message),
        }
    }
}

/// A model being downloaded.
pub(crate) struct ModelDownload {
    pub done: u64,
    pub total: Option<u64>,
    pub(crate) cancel: Arc<AtomicBool>,
    /// The model's files, claimed for as long as the download is wanted:
    /// let go when it is cancelled, not when its transfer has wound down —
    /// a Download straight after a Cancel was refused as "downloading in
    /// another window". The transfer still stopping writes a partial file
    /// of its own, and puts nothing in place once cancelled.
    pub(crate) _claim: Claim,
    /// Aborts the transfer when dropped — when the window closes with it
    /// running.
    pub(crate) _task: iced::task::Handle,
}

/// One of the voice settings: what is written when it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pref {
    Model,
    Language,
    RunCommands,
    SendQuestions,
    TalkOver,
    /// Start listening when called ([`VoicePrefs::wake`]).
    Wake,
    Speak,
    Speed,
    /// The voice picked for this speech model.
    Voice(&'static str),
}

/// The user's voice settings, kept in config.toml's `[voice]`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VoicePrefs {
    /// The model listened with ([`models::Model::id`]).
    pub model: String,
    /// The language spoken, as whisper's code; `None` to let a multilingual
    /// model tell.
    pub language: Option<String>,
    /// Run what plainly names a command.
    pub run_commands: bool,
    /// Ask what is not one at once, rather than leave it in the input.
    pub send_questions: bool,
    /// Let the reader talk over Clew while it speaks: the microphone stays
    /// open, Clew's own voice taken out of what it hears
    /// ([`clew_voice::echo`]). Off, it waits until Clew is done.
    pub talk_over: bool,
    /// Wait for "Hey, Clew" whenever the microphone is not listening, and
    /// listen once it is said ([`crate::app::wake`]).
    pub wake: bool,
    /// Which answers are read aloud ([`crate::app::speech`]).
    pub read_aloud: ReadAloud,
    /// How fast Clew speaks: 1 is the voice's own pace.
    pub speed: f32,
    /// The voice picked for each speech model, by the model's id.
    pub voices: std::collections::BTreeMap<String, i32>,
}

impl Default for VoicePrefs {
    fn default() -> Self {
        Self {
            model: models::DEFAULT_MODEL.to_string(),
            language: None,
            run_commands: true,
            send_questions: true,
            talk_over: true,
            wake: false,
            read_aloud: ReadAloud::Spoken,
            speed: 1.0,
            voices: std::collections::BTreeMap::new(),
        }
    }
}

/// Which of Clew's answers it reads aloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadAloud {
    Never,
    /// The answers to questions said aloud.
    Spoken,
    /// Every answer, to questions typed too.
    Always,
}

impl ReadAloud {
    pub(crate) const ALL: [ReadAloud; 3] = [ReadAloud::Never, ReadAloud::Spoken, ReadAloud::Always];

    /// Whether the answer to a question said aloud (`spoken`), or typed,
    /// is read aloud.
    pub(crate) fn reads(self, spoken: bool) -> bool {
        match self {
            ReadAloud::Never => false,
            ReadAloud::Spoken => spoken,
            ReadAloud::Always => true,
        }
    }

    /// Its name, in English — translated where it is shown.
    pub(crate) fn name(self) -> &'static str {
        match self {
            ReadAloud::Never => tr_noop!("Never"),
            ReadAloud::Spoken => tr_noop!("For spoken questions"),
            ReadAloud::Always => tr_noop!("Always"),
        }
    }

    /// As config.toml keeps it.
    fn key(self) -> &'static str {
        match self {
            ReadAloud::Never => "never",
            ReadAloud::Spoken => "spoken",
            ReadAloud::Always => "always",
        }
    }

    /// As config.toml keeps it — or as it kept it, a switch: on read the
    /// answers to questions said aloud.
    fn from_value(value: &toml::Value) -> Option<Self> {
        match value {
            toml::Value::Boolean(true) => Some(ReadAloud::Spoken),
            toml::Value::Boolean(false) => Some(ReadAloud::Never),
            toml::Value::String(key) => Self::ALL.into_iter().find(|r| r.key() == key),
            _ => None,
        }
    }
}

impl std::fmt::Display for ReadAloud {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(crate::i18n::lookup(self.name()))
    }
}

/// The languages offered for a multilingual model, as whisper's codes and
/// their names — in English, shown through `crate::i18n::lookup`; `None`
/// lets the model tell.
pub(crate) const LANGUAGES: &[(Option<&str>, &str)] = &[
    (None, tr_noop!("Detect automatically")),
    (Some("en"), tr_noop!("English")),
    (Some("zh"), tr_noop!("Chinese")),
    (Some("es"), tr_noop!("Spanish")),
    (Some("fr"), tr_noop!("French")),
    (Some("de"), tr_noop!("German")),
    (Some("ja"), tr_noop!("Japanese")),
    (Some("ko"), tr_noop!("Korean")),
    (Some("pt"), tr_noop!("Portuguese")),
    (Some("ru"), tr_noop!("Russian")),
    (Some("it"), tr_noop!("Italian")),
    (Some("hi"), tr_noop!("Hindi")),
];

impl VoicePrefs {
    pub(crate) fn load() -> Self {
        let mut prefs = Self::default();
        let Some(section) = clew_core::globalconfig::section("voice") else {
            return prefs;
        };
        if let Some(model) = section
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|id| models::find(id).is_some())
        {
            prefs.model = model.to_string();
        }
        prefs.language = section
            .get("language")
            .and_then(|v| v.as_str())
            .filter(|l| LANGUAGES.iter().any(|(code, _)| *code == Some(*l)))
            .map(str::to_string);
        if let Some(on) = section.get("commands").and_then(|v| v.as_bool()) {
            prefs.run_commands = on;
        }
        if let Some(on) = section.get("send").and_then(|v| v.as_bool()) {
            prefs.send_questions = on;
        }
        if let Some(on) = section.get("talk_over").and_then(|v| v.as_bool()) {
            prefs.talk_over = on;
        }
        if let Some(on) = section.get("wake").and_then(|v| v.as_bool()) {
            prefs.wake = on;
        }
        if let Some(read_aloud) = section.get("speak").and_then(ReadAloud::from_value) {
            prefs.read_aloud = read_aloud;
        }
        if let Some(speed) = section
            .get("speed")
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
            .map(|v| v as f32)
            .filter(|v| (0.5..=2.0).contains(v))
        {
            prefs.speed = speed;
        }
        if let Some(voices) = section.get("voices").and_then(|v| v.as_table()) {
            for (id, voice) in voices {
                let Some(model) = clew_voice::speech::find(id) else {
                    continue;
                };
                if let Some(voice) = voice
                    .as_integer()
                    .and_then(|v| i32::try_from(v).ok())
                    .filter(|v| model.voices.iter().any(|known| known.id == *v))
                {
                    prefs.voices.insert(id.clone(), voice);
                }
            }
        }
        prefs
    }

    /// Write `pref` as it is now — that setting alone, into `[voice]` as
    /// it stands on disk: another window's settings, changed since this one
    /// read them, are kept.
    fn save(&self, pref: Pref) -> Result<(), String> {
        clew_core::globalconfig::edit(|root| {
            if !root.get("voice").is_some_and(|v| v.is_table()) {
                root.insert("voice".into(), toml::Table::new().into());
            }
            let Some(section) = root.get_mut("voice").and_then(|v| v.as_table_mut()) else {
                return;
            };
            match pref {
                Pref::Model => {
                    section.insert("model".into(), self.model.clone().into());
                }
                Pref::Language => match &self.language {
                    Some(language) => {
                        section.insert("language".into(), language.clone().into());
                    }
                    None => {
                        section.remove("language");
                    }
                },
                Pref::RunCommands => {
                    section.insert("commands".into(), self.run_commands.into());
                }
                Pref::SendQuestions => {
                    section.insert("send".into(), self.send_questions.into());
                }
                Pref::TalkOver => {
                    section.insert("talk_over".into(), self.talk_over.into());
                }
                Pref::Wake => {
                    section.insert("wake".into(), self.wake.into());
                }
                Pref::Speak => {
                    section.insert("speak".into(), self.read_aloud.key().into());
                }
                Pref::Speed => {
                    section.insert("speed".into(), f64::from(self.speed).into());
                }
                Pref::Voice(model) => {
                    if !section.get("voices").is_some_and(|v| v.is_table()) {
                        section.insert("voices".into(), toml::Table::new().into());
                    }
                    if let (Some(voices), Some(voice)) = (
                        section.get_mut("voices").and_then(|v| v.as_table_mut()),
                        self.voices.get(model),
                    ) {
                        voices.insert(model.into(), i64::from(*voice).into());
                    }
                }
            }
        })
    }

    /// The voice `model` speaks in: the one picked, else its own default.
    pub(crate) fn voice_for(&self, model: &clew_voice::speech::VoiceModel) -> i32 {
        self.voices
            .get(model.id)
            .copied()
            .filter(|v| model.voices.iter().any(|known| known.id == *v))
            .unwrap_or(model.default_voice)
    }

    /// The model chosen, from the catalog.
    pub(crate) fn model(&self) -> &'static Model {
        models::find(&self.model)
            .or_else(|| models::find(models::DEFAULT_MODEL))
            .unwrap_or(&models::MODELS[0])
    }
}

/// Voice's state in a window.
pub(crate) struct VoiceState {
    pub phase: VoicePhase,
    /// The microphone, while listening, and the model what it hears is
    /// for: the one found on this computer when listening began, whatever
    /// another window picks meanwhile.
    listening: Option<(Box<dyn Listening>, &'static Model)>,
    /// The microphone is on: it listens, one utterance after another, until
    /// its button or the voice key is pressed again. Closed meanwhile only
    /// while Clew reads an answer aloud ([`App::resume_listening`]).
    pub mic_on: bool,
    /// What was said, waiting to be worked out after what was said before
    /// it — one at a time, in the order it was said — with the model it is
    /// worked out with.
    unheard: VecDeque<(Recording, &'static Model, bool)>,
    /// What is being worked out began with Clew called by its name: what
    /// was heard before the name is not what was asked.
    run_called: bool,
    /// Where, in what the microphone is hearing, speech said over Clew
    /// began, in seconds: what is heard before it — what was left of Clew's
    /// voice — is not worked out with it.
    over_from: Option<f32>,
    /// The run working out what was said, by its token.
    transcribing: Option<u64>,
    /// The last run started.
    token: u64,
    /// When the voice key last started or stopped listening.
    last_toggle: Option<Instant>,
    /// What was last heard, and what came of it.
    pub heard: Option<Heard>,
    /// When it was heard: the pill in the corner says it for a moment
    /// ([`HEARD_SHOWN`]).
    pub heard_at: Option<Instant>,
    /// A command heard that asks before it runs.
    pub confirm: Option<Waiting>,
    /// Models downloading, by id.
    pub downloads: HashMap<&'static str, ModelDownload>,
    /// The last download's failure, by model id.
    pub download_errors: HashMap<&'static str, String>,
    pub prefs: VoicePrefs,
    /// Where models are kept: under the data directory.
    pub store: Option<Store>,
    pub(crate) open_mic: OpenMic,
    pub(crate) transcriber: Arc<dyn Transcriber>,
    pub(crate) fetch: Fetch,
    /// Clew's answers, read aloud.
    pub speech: crate::app::speech::Speech,
    /// Smart Turn: when the speaker has finished.
    pub(crate) turn: crate::app::turn::TurnState,
    /// Waiting for "Hey, Clew".
    pub(crate) wake: crate::app::wake::WakeState,
}

/// How long the pill in the corner says what was last heard.
pub(crate) const HEARD_SHOWN: Duration = Duration::from_secs(4);

impl VoiceState {
    /// Record what was heard, and when: the pill says it for a moment.
    pub(crate) fn hear(&mut self, heard: Heard) {
        self.heard = Some(heard);
        self.heard_at = Some(Instant::now());
    }

    /// Whether what was last heard is still said on the pill.
    pub(crate) fn heard_fresh(&self) -> bool {
        self.heard.is_some() && self.heard_at.is_some_and(|at| at.elapsed() < HEARD_SHOWN)
    }

    pub(crate) fn new() -> Self {
        Self {
            phase: VoicePhase::Idle,
            listening: None,
            mic_on: false,
            unheard: VecDeque::new(),
            run_called: false,
            over_from: None,
            transcribing: None,
            token: 0,
            last_toggle: None,
            heard: None,
            heard_at: None,
            confirm: None,
            downloads: HashMap::new(),
            download_errors: HashMap::new(),
            prefs: VoicePrefs::load(),
            store: clew_core::lsp::store::data_root().map(|d| Store::new(d.join("voice"))),
            open_mic: open_system_mic,
            transcriber: ENGINE.clone(),
            fetch: fetch_over_net,
            speech: crate::app::speech::Speech::new(),
            turn: crate::app::turn::TurnState::new(),
            wake: crate::app::wake::WakeState::new(),
        }
    }

    /// Whether Esc has something of voice's to put away: listening, a
    /// transcription, a command waiting for a yes, Clew speaking.
    /// Whether Esc is voice's: a command waits for a yes, or Clew speaks.
    /// Not the microphone: on, it stays on until its button turns it off.
    pub(crate) fn takes_esc(&self) -> bool {
        self.confirm.is_some() || self.speech.is_speaking()
    }

    pub(crate) fn is_listening(&self) -> bool {
        matches!(self.phase, VoicePhase::Listening { .. })
    }

    /// The microphone, while it listens.
    pub(crate) fn listening_now(&self) -> Option<&dyn Listening> {
        self.listening.as_ref().map(|(l, _)| l.as_ref())
    }

    /// Back to idle, as before anything was said — the key's last press
    /// forgotten too, so a test's next press is not taken for a repeat. A
    /// command waiting for a yes still waits, as it does for a real press.
    #[cfg(test)]
    pub(crate) fn reset_for_test(&mut self) {
        self.phase = VoicePhase::Idle;
        self.listening = None;
        self.mic_on = false;
        self.heard = None;
        self.last_toggle = None;
    }

    /// The voice key's last press forgotten, so a test's next press is not
    /// taken for a repeat.
    #[cfg(test)]
    pub(crate) fn forget_last_press(&mut self) {
        self.last_toggle = None;
    }

    /// Whether what was said is still being worked out, or waits to be.
    #[cfg(test)]
    pub(crate) fn transcribing(&self) -> bool {
        self.transcribing.is_some() || !self.unheard.is_empty()
    }

    /// Whether `model` is downloaded.
    pub(crate) fn installed(&self, model: &Model) -> bool {
        self.store.as_ref().is_some_and(|s| s.installed(model))
    }
}

/// Voice's messages.
#[derive(Debug, Clone)]
pub enum VoiceMsg {
    /// The voice key or the microphone button: turn the microphone on, or
    /// off — or, while Clew speaks, talk over it.
    Toggle,
    /// What is being said has ended: work it out, and listen on.
    Stop,
    /// Esc, or the Cancel of a command waiting for a yes: say no to it — or
    /// stop Clew speaking.
    Cancel,
    /// While listening: read the level and look for the pause that ends it.
    Tick,
    /// What Smart Turn said of pause `pause` of listening `listen`: how
    /// likely it is that the speaker has finished, or why it could not say.
    Judged {
        listen: u64,
        pause: u64,
        result: Result<f32, String>,
    },
    /// Download Smart Turn (the button that says its size).
    DownloadTurn,
    CancelTurnDownload,
    TurnDownloadProgress {
        done: u64,
        total: Option<u64>,
    },
    TurnDownloaded(Result<(), String>),
    /// Delete Smart Turn.
    RemoveTurn,
    /// Smart Turn deleted, or why it could not be.
    TurnRemoved(Result<(), String>),
    /// Wait for "Hey, Clew" whenever the microphone is not listening
    /// ([`VoicePrefs::wake`]).
    SetWake(bool),
    /// "Hey, Clew", heard by this waiting ([`crate::app::wake`]).
    Called(u64),
    /// This waiting failed, saying why.
    WaitFailed {
        wait: u64,
        error: String,
    },
    /// Download the keyword spotter (the button that says its size).
    DownloadWake,
    CancelWakeDownload,
    WakeDownloadProgress {
        done: u64,
        total: Option<u64>,
    },
    WakeDownloaded(Result<(), String>),
    /// Delete the keyword spotter.
    RemoveWake,
    /// The keyword spotter deleted, or why it could not be.
    WakeRemoved(Result<(), String>),
    /// What a run heard: its words, or why there are none.
    Transcribed {
        token: u64,
        result: Result<String, String>,
    },
    /// Yes to the command waiting for one.
    Confirm,
    /// Download a model (the user's go-ahead: the button that says its size).
    Download(&'static str),
    /// Stop downloading one.
    CancelDownload(&'static str),
    DownloadProgress {
        id: &'static str,
        done: u64,
        total: Option<u64>,
    },
    Downloaded {
        id: &'static str,
        result: Result<PathBuf, String>,
    },
    /// Delete a downloaded model.
    RemoveModel(&'static str),
    /// Listen with this model.
    PickModel(&'static str),
    /// The language a multilingual model hears; `None` to let it tell.
    SetLanguage(Option<&'static str>),
    SetRunCommands(bool),
    SetSendQuestions(bool),
    /// Let the reader talk over Clew ([`VoicePrefs::talk_over`]).
    SetTalkOver(bool),
    /// A sentence made into speech, for the stop round it was made in — or
    /// why it could not be.
    Spoken {
        round: u64,
        result: Result<crate::app::speech::Audio, clew_voice::speaker::SayError>,
        /// The output, opened meanwhile when none was open.
        opened: Option<crate::app::speech::Opened>,
    },
    /// While Clew speaks: the meter, and whether all is said.
    SpeakTick,
    /// Stop speaking.
    StopSpeaking,
    /// Which answers are read aloud.
    SetSpeak(ReadAloud),
    /// How fast Clew speaks.
    SetSpeed(f32),
    /// Speak with this voice of this speech model.
    PickSpeechVoice {
        model: &'static str,
        voice: i32,
    },
    /// Say a sample with this speech model.
    TrySpeech(&'static str),
    /// Download a speech model (the button that says its size).
    DownloadSpeech(&'static str),
    CancelSpeechDownload(&'static str),
    SpeechDownloadProgress {
        id: &'static str,
        done: u64,
        total: Option<u64>,
    },
    SpeechDownloaded {
        id: &'static str,
        result: Result<(), String>,
    },
    /// Delete a downloaded speech model.
    RemoveSpeech(&'static str),
    /// A speech model deleted, or why it could not be.
    SpeechRemoved {
        id: &'static str,
        result: Result<(), String>,
    },
}

impl VoiceMsg {
    /// None of voice's messages is tied to the project open when it was
    /// sent: words heard are meant for the window as it is, and a model is
    /// the machine's. Runs are ordered by their own token.
    pub fn origin(&self) -> Option<Origin<'_>> {
        None
    }
}

/// The index in `shown` of the command `said` names in `lang`, when it is
/// one of the titles as the palette shows them — said with or without its
/// category ("View:") or its aside in parentheses, and with the words that
/// only say "do it" or "please" (open, show, go to; 打开, 请; を開いて,
/// 열어 줘) left out on both sides. Spaces and punctuation aside, what is
/// left must be the title exactly, and only one title: an extra word is a
/// request for Ask, not a command. Words that put something away (close,
/// hide) are never dropped, so "close the minimap" never shows it. English
/// is `clew_voice::intent::command`'s, which reads it word by word.
pub(crate) fn said_as_shown(lang: crate::i18n::Lang, said: &str, shown: &[&str]) -> Option<usize> {
    use crate::i18n::Lang;
    // Each list is compacted as the text is, so "s'il vous plaît" is
    // "silvousplaît".
    let (leading, trailing): (&[&str], &[&str]) = match lang {
        Lang::En => return None,
        Lang::Fr => (
            &[
                "silvousplaît",
                "silteplaît",
                "pouvezvous",
                "peuxtu",
                "ouvrir",
                "ouvre",
                "ouvrez",
                "afficher",
                "affiche",
                "affichez",
                "montrer",
                "montre",
                "aller",
                "allez",
                "va",
                "à",
                "au",
                "aux",
                "les",
                "le",
                "la",
                "l",
                "un",
                "une",
                "des",
            ],
            &["silvousplaît", "silteplaît", "merci"],
        ),
        Lang::Zh => (
            &[
                "麻烦",
                "帮我",
                "给我",
                "我想",
                "我要",
                "请",
                "打开",
                "显示",
                "转到",
                "跳转到",
                "切换到",
                "去",
            ],
            &["一下", "吧", "谢谢"],
        ),
        Lang::Ko => (
            &[],
            &[
                "열어주세요",
                "열어줘",
                "열어",
                "보여주세요",
                "보여줘",
                "표시해주세요",
                "표시해줘",
                "해주세요",
                "해줘",
                "주세요",
                "열기",
                "표시",
                "을",
                "를",
            ],
        ),
        Lang::Ja => (
            &[],
            &[
                "してください",
                "ください",
                "を開いて",
                "開いて",
                "を開く",
                "開く",
                "を表示して",
                "表示して",
                "を表示",
                "して",
                "を",
                "に",
            ],
        ),
    };
    fn compact(text: &str) -> String {
        text.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    }
    // Each time the longest word that fits, whatever the lists' order:
    // "ouvrez" is not "ouvre" and a "z", nor "aux" "au" and an "x".
    let bare = |text: &str| -> String {
        let mut text = compact(text);
        loop {
            let before = text.len();
            if let Some(word) = leading
                .iter()
                .filter(|word| text.starts_with(**word) && text.len() > word.len())
                .max_by_key(|word| word.len())
            {
                text = text[word.len()..].to_string();
            }
            if let Some(word) = trailing
                .iter()
                .filter(|word| text.ends_with(**word) && text.len() > word.len())
                .max_by_key(|word| word.len())
            {
                text = text[..text.len() - word.len()].to_string();
            }
            if text.len() == before {
                return text;
            }
        }
    };
    let said = bare(said);
    if said.is_empty() {
        return None;
    }
    // The title as it is written is a better fit than the title without its
    // category: "Raccourcis clavier" is "Raccourcis clavier…", not
    // "Réglages : Raccourcis clavier". Two equally good: neither.
    let mut found: Option<(usize, usize)> = None;
    let mut tied = false;
    for (index, title) in shown.iter().enumerate() {
        // The title, without its category, without its aside, both.
        let main = title
            .split_once([':', '：'])
            .map_or(*title, |(_, rest)| rest);
        let plain = |t: &str| -> String {
            match t.split_once(['(', '（']) {
                Some((before, _)) => before.to_string(),
                None => t.to_string(),
            }
        };
        let fit = if [*title, &plain(title)]
            .iter()
            .any(|form| bare(form) == said)
        {
            0
        } else if [main, &plain(main)].iter().any(|form| bare(form) == said) {
            1
        } else {
            continue;
        };
        match found {
            Some((best, _)) if best < fit => {}
            Some((best, _)) if best == fit => tied = true,
            _ => {
                found = Some((fit, index));
                tied = false;
            }
        }
    }
    if tied { None } else { found.map(|(_, i)| i) }
}

/// Whether `heard`, said while Clew spoke, is the reader's own words —
/// not Clew's (`said`, its last sentences) heard back, which the echo
/// canceller can leave a little of. A few words, nearly all of them Clew's,
/// are its echo; anything short is the reader's ("stop", "wait"), whatever
/// Clew said.
pub(crate) fn said_over(heard: &str, said: &[String]) -> bool {
    let words = word_tokens(heard);
    if words.is_empty() {
        return false;
    }
    if words.len() < ECHO_WORDS {
        return true;
    }
    let clews: std::collections::HashSet<String> =
        said.iter().flat_map(|s| word_tokens(s)).collect();
    let echoed = words.iter().filter(|w| clews.contains(*w)).count();
    echoed * 10 < words.len() * 7
}

/// Fewest words an echo of Clew is taken to be: anything shorter said over
/// it is the reader's.
const ECHO_WORDS: usize = 3;

/// The words of `text`, lower-cased: runs of letters and digits — and each
/// character of Chinese, Japanese and Korean, written without spaces.
fn word_tokens(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    for c in text.chars() {
        let cjk = matches!(c as u32,
            0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF);
        if cjk {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            words.push(c.to_string());
        } else if c.is_alphanumeric() {
            word.extend(c.to_lowercase());
        } else if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

impl App {
    /// Handle a [`VoiceMsg`].
    pub(crate) fn update_voice(&mut self, message: VoiceMsg) -> Task<Message> {
        match message {
            message @ (VoiceMsg::Spoken { .. }
            | VoiceMsg::SpeakTick
            | VoiceMsg::StopSpeaking
            | VoiceMsg::SetSpeak(_)
            | VoiceMsg::SetSpeed(_)
            | VoiceMsg::PickSpeechVoice { .. }
            | VoiceMsg::TrySpeech(_)
            | VoiceMsg::DownloadSpeech(_)
            | VoiceMsg::CancelSpeechDownload(_)
            | VoiceMsg::SpeechDownloadProgress { .. }
            | VoiceMsg::SpeechDownloaded { .. }
            | VoiceMsg::RemoveSpeech(_)
            | VoiceMsg::SpeechRemoved { .. }) => self.update_speech(message),
            VoiceMsg::Toggle => self.on_voice_toggle(),
            VoiceMsg::Stop => self.end_utterance(),
            VoiceMsg::Cancel => {
                self.on_voice_escape();
                Task::none()
            }
            VoiceMsg::Tick => self.on_voice_tick(),
            VoiceMsg::Judged {
                listen,
                pause,
                result,
            } => self.on_judged(listen, pause, result),
            message @ (VoiceMsg::DownloadTurn
            | VoiceMsg::CancelTurnDownload
            | VoiceMsg::TurnDownloadProgress { .. }
            | VoiceMsg::TurnDownloaded(_)
            | VoiceMsg::RemoveTurn
            | VoiceMsg::TurnRemoved(_)) => self.on_turn_settings(message),
            message @ (VoiceMsg::SetWake(_)
            | VoiceMsg::Called(_)
            | VoiceMsg::WaitFailed { .. }
            | VoiceMsg::DownloadWake
            | VoiceMsg::CancelWakeDownload
            | VoiceMsg::WakeDownloadProgress { .. }
            | VoiceMsg::WakeDownloaded(_)
            | VoiceMsg::RemoveWake
            | VoiceMsg::WakeRemoved(_)) => self.on_wake(message),
            VoiceMsg::Transcribed { token, result } => self.on_transcribed(token, result),
            VoiceMsg::Confirm => match self.voice.confirm.take() {
                Some(waiting) => {
                    self.voice.heard = self.voice.heard.take().map(|h| Heard {
                        outcome: HeardOutcome::Ran(waiting.done()),
                        ..h
                    });
                    self.run_waiting(waiting)
                }
                None => Task::none(),
            },
            VoiceMsg::Download(id) => self.start_model_download(id),
            VoiceMsg::CancelDownload(id) => {
                if let Some(d) = self.voice.downloads.remove(id) {
                    d.cancel.store(true, Ordering::Relaxed);
                }
                Task::none()
            }
            VoiceMsg::DownloadProgress { id, done, total } => {
                if let Some(d) = self.voice.downloads.get_mut(id) {
                    d.done = done;
                    d.total = total;
                }
                Task::none()
            }
            VoiceMsg::Downloaded { id, result } => {
                // Cancelled meanwhile: what it says is nobody's.
                if self.voice.downloads.remove(id).is_none() {
                    return Task::none();
                }
                let name = crate::i18n::lookup(models::find(id).map_or(id, |m| m.name));
                match result {
                    Ok(_) => {
                        self.voice.download_errors.remove(id);
                        // The model just downloaded is the one the voice
                        // key hears with when the one chosen is not on this
                        // computer (the first one chosen, Small, is not):
                        // kept to it, the key sent the reader back to
                        // Settings for it. One chosen and here stays chosen
                        // — one picked while this downloaded included.
                        let chosen = self.voice.prefs.model();
                        // Nor one chosen and still on its way, in any window.
                        let coming = self
                            .voice
                            .store
                            .as_ref()
                            .is_some_and(|store| downloading(&store.path(chosen)));
                        let switch = chosen.id != id && !self.voice.installed(chosen) && !coming;
                        self.status = if switch || chosen.id == id {
                            tr!("{model} is ready", model = name)
                        } else {
                            tr!("{model} is downloaded", model = name)
                        };
                        if switch {
                            self.voice.prefs.model = id.to_string();
                            self.save_voice_pref(Pref::Model);
                        }
                    }
                    Err(e) => {
                        self.status = tr!(
                            "Could not download {model}: {error}",
                            model = name,
                            error = e
                        );
                        self.voice.download_errors.insert(id, e);
                    }
                }
                Task::none()
            }
            VoiceMsg::RemoveModel(id) => {
                let mut rechosen = false;
                if let (Some(model), Some(store)) = (models::find(id), self.voice.store.as_ref()) {
                    let name = crate::i18n::lookup(model.name);
                    self.status = match store.remove(model) {
                        Ok(()) => {
                            // The model the voice key hears with, removed
                            // while another is still on this computer: that
                            // one is used now, rather than the key asking to
                            // download again the one just removed.
                            if self.voice.prefs.model == model.id
                                && let Some(other) =
                                    models::MODELS.iter().find(|m| store.installed(m))
                            {
                                self.voice.prefs.model = other.id.to_string();
                                rechosen = true;
                            }
                            tr!("Removed {model}", model = name)
                        }
                        Err(e) => tr!("Could not remove {model}: {error}", model = name, error = e),
                    };
                }
                if rechosen {
                    self.save_voice_pref(Pref::Model);
                }
                Task::none()
            }
            VoiceMsg::PickModel(id) => {
                if models::find(id).is_some() {
                    self.voice.prefs.model = id.to_string();
                    self.save_voice_pref(Pref::Model);
                }
                Task::none()
            }
            VoiceMsg::SetLanguage(language) => {
                self.voice.prefs.language = language.map(str::to_string);
                self.save_voice_pref(Pref::Language);
                Task::none()
            }
            VoiceMsg::SetRunCommands(on) => {
                self.voice.prefs.run_commands = on;
                self.save_voice_pref(Pref::RunCommands);
                Task::none()
            }
            VoiceMsg::SetSendQuestions(on) => {
                self.voice.prefs.send_questions = on;
                self.save_voice_pref(Pref::SendQuestions);
                Task::none()
            }
            VoiceMsg::SetTalkOver(on) => {
                self.voice.prefs.talk_over = on;
                self.save_voice_pref(Pref::TalkOver);
                // The microphone open now was opened as it was then: open
                // it again as it is now — or, Clew speaking, let it wait.
                if self.voice.phase != VoicePhase::Idle
                    && self
                        .voice
                        .listening_now()
                        .is_some_and(|l| l.silence_after_speech().is_none())
                {
                    self.close_mic();
                    if self.voice.speech.is_speaking() && !on {
                        self.voice.speech.go_on();
                    } else {
                        self.resume_listening();
                    }
                }
                // So is the one waiting for "Hey, Clew".
                if self.voice.wake.is_waiting() {
                    self.stop_waiting();
                    self.resume_waiting();
                }
                Task::none()
            }
        }
    }

    pub(crate) fn save_voice_pref(&mut self, pref: Pref) {
        if let Err(e) = self.voice.prefs.save(pref) {
            self.status = tr!("Could not save the voice settings: {error}", error = e);
        }
    }

    /// The voice key or the microphone button: the microphone on, or off.
    /// Pressed while Clew speaks, it is said over Clew, as over a person:
    /// Clew stops, and listens.
    fn on_voice_toggle(&mut self) -> Task<Message> {
        let now = Instant::now();
        // A held key repeats (as the menu's key equivalent, too): every
        // repeat is as close to the last as a repeat is, and is ignored.
        if self
            .voice
            .last_toggle
            .is_some_and(|t| now.duration_since(t) < TOGGLE_DEBOUNCE)
        {
            self.voice.last_toggle = Some(now);
            return Task::none();
        }
        self.voice.last_toggle = Some(now);
        if self.voice.speech.is_speaking() {
            // With the microphone on, it opens as Clew stops.
            self.stop_speaking();
            if self.voice.mic_on {
                return Task::none();
            }
            return self.start_listening();
        }
        if self.voice.mic_on {
            return self.mic_off();
        }
        // An answer still coming that would be read aloud is talked over:
        // it is not read, and the microphone does not wait for it.
        if self.answer_read_aloud_coming() {
            self.stop_speaking();
        }
        self.start_listening()
    }

    /// The microphone turned off. What it is hearing someone say is kept —
    /// the press ends it, as a pause would — and what was said before is
    /// still worked out and acted on; a listen that has heard nothing yet
    /// is put away.
    ///
    /// Called by its name ([`VoicePrefs::wake`]), it waits for it again —
    /// with the microphone that listened, if it is still open.
    fn mic_off(&mut self) -> Task<Message> {
        self.voice.mic_on = false;
        self.status = tr!("Stopped listening").into();
        let hearing = self
            .voice
            .listening_now()
            .is_some_and(|l| l.silence_after_speech().is_some());
        let task = if hearing {
            self.end_utterance_now()
        } else {
            let mic = self.voice.listening.take().map(|(mic, _)| mic);
            self.close_mic();
            self.wait_with(mic);
            Task::none()
        };
        if self.resume_waiting() {
            self.status = tr!("Stopped listening — say “Hey, Clew” to start again").into();
        }
        task
    }

    /// Clew is about to speak — an answer it reads aloud is coming, Settings'
    /// Try: the microphone, on, waits until all is said
    /// ([`App::resume_listening`]), or it would hear Clew. Someone being
    /// heard is heard out first.
    pub(crate) fn hold_mic_for_clew(&mut self) {
        // Talked over, Clew is heard through: the microphone stays open.
        if self.voice.prefs.talk_over {
            return;
        }
        let hearing = self
            .voice
            .listening_now()
            .is_some_and(|l| l.silence_after_speech().is_some());
        if !hearing {
            self.close_mic();
        }
    }

    /// Close the microphone, keeping nothing of what it heard — nor what
    /// was heard before it was called.
    fn close_mic(&mut self) {
        if let Some((listening, _)) = self.voice.listening.take() {
            drop(listening.stop());
        }
        self.voice.wake.lead = None;
        self.voice.phase = VoicePhase::Idle;
    }

    /// With the microphone on, once nothing keeps it closed, it opens for
    /// what is said next — at once after what was said last, which is
    /// worked out meanwhile. Not while Clew speaks, nor while an answer it
    /// will read aloud is still coming: it would hear Clew. Once all is
    /// said, or the answer is over (`on_ask_stream_ended`), this is asked
    /// again. And only with a model to hear with: this never sends the
    /// reader to Settings for one.
    pub(crate) fn resume_listening(&mut self) {
        if !self.voice.mic_on || self.voice.phase != VoicePhase::Idle {
            return;
        }
        if !self.voice.prefs.talk_over
            && (self.voice.speech.is_speaking() || self.answer_read_aloud_coming())
        {
            return;
        }
        if !self.voice.installed(self.voice.prefs.model()) {
            // Removed meanwhile: the microphone is off, as it is.
            self.voice.mic_on = false;
            return;
        }
        self.open_mic();
    }

    /// Turn the microphone on — once there is a model to hear it with. It
    /// listens until it is turned off.
    fn start_listening(&mut self) -> Task<Message> {
        self.listen_now(false)
    }

    /// [`App::start_listening`] — `called` by its name, "Hey, Clew", the
    /// moment the microphone heard before is kept, as the lead of what is
    /// said next.
    pub(crate) fn listen_now(&mut self, called: bool) -> Task<Message> {
        let model = self.voice.prefs.model();
        if !self.voice.installed(model) {
            // Another one here: it is not that there is none.
            let another = models::MODELS.iter().any(|m| self.voice.installed(m));
            self.status = if another {
                tr!(
                    "{model} is not downloaded yet — download it ({size} MB) in Settings → \
                     Voice, or choose one that is",
                    model = crate::i18n::lookup(model.name),
                    size = model.size_mb
                )
            } else {
                tr!(
                    "Voice needs a speech model on this computer — download {model} ({size} MB) \
                     in Settings → Voice",
                    model = crate::i18n::lookup(model.name),
                    size = model.size_mb
                )
            };
            return self.update(Message::Settings(SettingsMsg::OpenAt(SettingsPage::Voice)));
        }
        // Not called: what was heard before the key is not the lead of
        // what is said now.
        let waiting = self.voice.wake.is_waiting();
        self.voice.mic_on = true;
        self.voice.wake.called = called;
        self.voice.heard = None;
        self.open_mic();
        if waiting && !called {
            self.voice.wake.lead = None;
        }
        if self.voice.mic_on {
            self.say_listening();
        }
        Task::none()
    }

    /// The status line, as listening begins.
    fn say_listening(&mut self) {
        // A command waiting for a yes keeps waiting: what is said next can
        // be the yes (or the no).
        self.status = match &self.voice.confirm {
            Some(waiting) => tr!(
                "Listening… say yes to run {command}, or no",
                command = waiting.said()
            ),
            None => tr!("Listening… say a command or a question").into(),
        };
    }

    /// Open the microphone for what is said next — the one waiting for
    /// "Hey, Clew", if one is: its echo canceller has learned the room. One
    /// that cannot be opened turns it off, saying why.
    pub(crate) fn open_mic(&mut self) {
        let model = self.voice.prefs.model();
        let opened = match self.take_waiting_mic() {
            Some(mic) => Ok(mic),
            None => (self.voice.open_mic)(MAX_SECONDS, self.voice.prefs.talk_over),
        };
        match opened {
            Ok(listening) => {
                self.voice.listening = Some((listening, model));
                self.voice.over_from = None;
                self.voice.phase = VoicePhase::Listening {
                    since: Instant::now(),
                    level: 0.0,
                };
                self.voice.wake.last_said = Instant::now();
                self.turn_listening_began();
            }
            Err(e) => {
                self.voice.mic_on = false;
                self.status = tr!("Can't listen: {error}", error = e);
                self.voice.hear(Heard {
                    text: String::new(),
                    outcome: HeardOutcome::Failed(e),
                });
            }
        }
    }

    /// While listening: the meter, and whether what is being said has
    /// ended.
    fn on_voice_tick(&mut self) -> Task<Message> {
        let VoicePhase::Listening { since, .. } = self.voice.phase else {
            return Task::none();
        };
        let Some((listening, _)) = self.voice.listening.as_ref() else {
            return Task::none();
        };
        if let Some(e) = listening.error() {
            self.voice.mic_on = false;
            self.close_mic();
            self.status = tr!("Stopped listening: {error}", error = e);
            return Task::none();
        }
        let level = listening.level();
        self.voice.phase = VoicePhase::Listening { since, level };
        let full = listening.full();
        let mut quiet = listening.silence_after_speech();
        let over = listening.speech_over();
        // Clew speaks, or an answer it reads aloud is on its way — the
        // question was just sent: the microphone waits until all is said —
        // unless the reader may talk over Clew, who is then held at the
        // first words said over it, while they are heard out.
        let speaking = self.voice.speech.is_speaking() || self.answer_read_aloud_coming();
        // Called by its name, and nothing said for a while — Clew quiet,
        // nothing being worked out or waiting for a yes: it waits for its
        // name again. Turned on by the voice key, it listens until turned
        // off.
        if self.voice.prefs.wake && self.voice.wake.called {
            let busy = speaking
                || quiet.is_some()
                || self.voice.transcribing.is_some()
                || self.voice.confirm.is_some()
                || self.ask_stream_active();
            if busy {
                self.voice.wake.last_said = Instant::now();
            } else if self.voice.wake.last_said.elapsed() >= crate::app::wake::IDLE {
                return self.mic_off();
            }
        }
        if speaking && !self.voice.prefs.talk_over && quiet.is_none() {
            self.hold_mic_for_clew();
            return Task::none();
        }
        // Said over Clew: only an unbroken run of speech holds it. Short of
        // one, what is heard is what is left of Clew's own voice, and ends
        // nothing.
        if self.voice.speech.is_speaking() && !self.voice.speech.held() {
            match over {
                Some(from) => {
                    self.voice.speech.hold();
                    self.voice.over_from = Some(from);
                    self.status = tr!("Listening… say a command or a question").into();
                }
                None => quiet = None,
            }
        }
        // The pause ends what is being said — at once when Smart Turn hears
        // a turn finished, later when it hears one that is not, after
        // [`END_SILENCE`] without it.
        let (paused, judge) = self.pause_ends(quiet);
        if full || paused {
            return self.end_utterance();
        }
        if quiet.is_some() {
            return judge;
        }
        if since.elapsed().as_secs_f32() >= NO_SPEECH_TIMEOUT {
            // Nothing said for a while: the quiet is let go, and the
            // microphone listens afresh for what is said next.
            if self.listen_on_afresh().is_none() {
                self.close_mic();
                self.open_mic();
            }
        }
        Task::none()
    }

    /// Of what was heard, what was said over Clew: from where it began, a
    /// little before — not what was heard of Clew's own voice before it.
    /// And what was heard just before Clew was called, before it: what was
    /// said in one breath with "Hey, Clew" is heard whole. Whether it was.
    fn said_from(&mut self, recording: &mut Recording) -> bool {
        if let Some(from) = self.voice.over_from.take() {
            let cut = ((from - OVER_LEAD).max(0.0) * recording.rate as f32) as usize;
            recording.samples.drain(..cut.min(recording.samples.len()));
        }
        match self.voice.wake.lead.take() {
            Some(lead) if lead.rate == recording.rate => {
                recording.samples.splice(0..0, lead.samples);
                true
            }
            _ => false,
        }
    }

    /// What the microphone heard so far, handed over, and it listening on
    /// afresh — the same one, kept open: opened anew, its echo canceller
    /// would learn the room again, and let Clew's voice through meanwhile.
    /// `None`, nothing taken, when it may not listen on as it is: turned
    /// off, to wait while Clew speaks, or opened for another model.
    fn listen_on_afresh(&mut self) -> Option<(Recording, bool)> {
        let model = self.voice.prefs.model();
        let waits = !self.voice.prefs.talk_over
            && (self.voice.speech.is_speaking() || self.answer_read_aloud_coming());
        if !self.voice.mic_on || waits || !self.voice.installed(model) {
            return None;
        }
        let (listening, opened_for) = self.voice.listening.as_mut()?;
        if !std::ptr::eq(*opened_for, model) {
            return None;
        }
        let mut recording = listening.take();
        let called = self.said_from(&mut recording);
        self.voice.phase = VoicePhase::Listening {
            since: Instant::now(),
            level: 0.0,
        };
        self.turn_listening_began();
        Some((recording, called))
    }

    /// What is being said has ended — a pause, Smart Turn, the most that
    /// is kept: it is worked out out of sight, after what was said before
    /// it, while the microphone listens on for what is said next.
    fn end_utterance(&mut self) -> Task<Message> {
        let model = self.voice.listening.as_ref().map(|(_, model)| *model);
        if let (Some((recording, called)), Some(model)) = (self.listen_on_afresh(), model) {
            self.voice.unheard.push_back((recording, model, called));
            return self.transcribe_next();
        }
        let task = self.end_utterance_now();
        self.resume_listening();
        task
    }

    /// What the microphone heard, taken to be worked out; the microphone
    /// closed.
    fn end_utterance_now(&mut self) -> Task<Message> {
        let Some((listening, model)) = self.voice.listening.take() else {
            return Task::none();
        };
        self.voice.phase = VoicePhase::Idle;
        let mut recording = listening.stop();
        let called = self.said_from(&mut recording);
        self.voice.unheard.push_back((recording, model, called));
        self.transcribe_next()
    }

    /// Work out the next of what was said, off the UI thread — once what
    /// was said before it is: one at a time, in the order said, each read
    /// with the window as the one before left it.
    fn transcribe_next(&mut self) -> Task<Message> {
        if self.voice.transcribing.is_some() {
            return Task::none();
        }
        let Some((recording, model, called)) = self.voice.unheard.pop_front() else {
            return Task::none();
        };
        self.voice.run_called = called;
        self.voice.token += 1;
        let token = self.voice.token;
        self.voice.transcribing = Some(token);
        let Some(path) = self.voice.store.as_ref().map(|s| s.path(model)) else {
            return self.on_transcribed(token, Err(tr!("no data directory for the model").into()));
        };
        // The window's command names, so whisper spells them as they are
        // spelled here — and only those: the longer the hint, the likelier
        // whisper is to hear it in what was not clearly said. The phrases
        // said with a value ("go to line 30") use the titles' words.
        let words: Vec<String> = self
            .palette_commands()
            .iter()
            // Not the voice key's own "Talk": nobody says it, and whisper
            // would hear it for "start".
            .filter(|c| {
                !matches!(
                    c.action,
                    crate::app::palette::PaletteAction::Send(Message::Editor(
                        EditorMsg::RunAction(crate::keymap::Action::Voice)
                    ))
                )
            })
            .map(|c| c.shown().trim_end_matches('…').to_string())
            // Clew's own name, as it is called by — not "clue".
            .chain(["Clew".to_string()])
            .collect();
        let hint = words.join(", ");
        let options = Options {
            language: self.voice.prefs.language.clone(),
            hint,
            threads: 0,
        };
        let transcriber = self.voice.transcriber.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    match clew_voice::audio::prepare(&recording.samples, recording.rate) {
                        // Nothing that sounded like speech: whisper is not
                        // asked to make words of the room.
                        None => Ok(String::new()),
                        Some(audio) => transcriber.transcribe(
                            &path,
                            audio.as_slice(),
                            &options,
                            &AtomicBool::new(false),
                        ),
                    }
                })
                .await
                .map_err(|_| tr!("transcribing failed unexpectedly").to_string())?
            },
            move |result| Message::Voice(VoiceMsg::Transcribed { token, result }),
        )
    }

    /// Esc, or the Cancel of a command waiting for a yes: no to that
    /// command — else Clew stops speaking. The microphone is left as it is.
    fn on_voice_escape(&mut self) {
        if let Some(waiting) = self.voice.confirm.take() {
            self.status = tr!("Didn't run {command}", command = waiting.said());
            self.voice.heard = None;
            return;
        }
        if self.stop_speaking() {
            self.status = tr!("Stopped speaking").into();
        }
    }

    /// What a run heard, then the next of what was said.
    fn on_transcribed(&mut self, token: u64, result: Result<String, String>) -> Task<Message> {
        if self.voice.transcribing != Some(token) {
            return Task::none();
        }
        self.voice.transcribing = None;
        // Said over Clew, which was held while it was heard: words — not
        // Clew's own, heard back — stop it, and are read as any are;
        // nothing, or Clew's own words, and it goes on where it was.
        if self.voice.speech.held() {
            let said_over = result
                .as_ref()
                .is_ok_and(|text| said_over(text, &self.voice.speech.said_lately()));
            if said_over {
                self.stop_speaking();
            } else {
                self.voice.speech.go_on();
                if result.is_ok() {
                    return self.transcribe_next();
                }
            }
        }
        let called = std::mem::take(&mut self.voice.run_called);
        let task = self.on_words(result, called);
        Task::batch([task, self.transcribe_next()])
    }

    /// The words heard: a command, run (or asked about); else a question
    /// for Ask. Nothing that could be read is let go without a word: the
    /// microphone listens on.
    /// Begun with Clew `called` by its name, what was heard before the name
    /// is not what was asked.
    fn on_words(&mut self, result: Result<String, String>, called: bool) -> Task<Message> {
        let text = match result {
            Ok(text) => text.trim().to_string(),
            Err(e) => {
                self.status = tr!("Could not transcribe: {error}", error = e);
                self.voice.hear(Heard {
                    text: String::new(),
                    outcome: HeardOutcome::Failed(e),
                });
                return Task::none();
            }
        };
        // Clew called by its name before what is said to it: what is said
        // is what follows ("Hey, Clew, open Settings").
        let named = if called {
            crate::app::spoken::after_name(&text)
        } else {
            crate::app::spoken::without_name(&text)
        };
        // "Bye, Clew", "Stop listening" — after the name or not: the
        // microphone turned off — or, called by its name, waiting for it
        // again. A command waiting for a yes is put away, as by anything
        // else said.
        if crate::app::spoken::stops_listening(&text) || crate::app::spoken::stops_listening(named)
        {
            self.voice.confirm = None;
            self.voice.hear(Heard {
                text,
                outcome: HeardOutcome::Ran(tr_noop!("Stop listening").into()),
            });
            return self.mic_off();
        }
        let text = named.to_string();
        if text.is_empty() {
            return Task::none();
        }
        // The answer to a command waiting for a yes; anything else said
        // instead puts it away and is read as itself.
        if let Some(waiting) = self.voice.confirm.take() {
            match crate::app::spoken::yes_or_no(&text) {
                Some(true) => {
                    self.status = tr!(
                        "“{text}” → {command}",
                        text = text,
                        command = waiting.said()
                    );
                    self.voice.hear(Heard {
                        text,
                        outcome: HeardOutcome::Ran(waiting.done()),
                    });
                    return self.run_waiting(waiting);
                }
                Some(false) => {
                    self.status = tr!("Didn't run {command}", command = waiting.said());
                    self.voice.heard = None;
                    return Task::none();
                }
                None => {}
            }
        }
        if self.voice.prefs.run_commands {
            if let Some(task) = self.heard_command(&text) {
                return task;
            }
        } else if let Some(task) = self.heard_commands_back_on(&text) {
            return task;
        }
        // A question: to Ask. Sent, it is answered where Ask is — the card
        // is not opened over the code for it: the pill says it is being
        // answered, and the answer is read aloud. Kept for the reader to
        // send, it waits in the card's input, open on it.
        self.voice.hear(Heard {
            text: text.clone(),
            outcome: HeardOutcome::Asked,
        });
        let mut input = self.proj.ask_input.trim_end().to_string();
        if !input.is_empty() {
            input.push(' ');
        }
        input.push_str(&text);
        self.proj.ask_input = input;
        self.proj.ask_spoken = true;
        if self.voice.prefs.send_questions {
            // While one is answered, it is queued, and asked next.
            self.status = tr!("Asking “{text}”", text = text);
            return self.update(Message::Ask(AskMsg::Submit));
        }
        self.reveal_ask();
        self.code_focused = false;
        Task::batch([
            operation::focus(ui::ask_input_id()),
            operation::move_cursor_to_end(ui::ask_input_id()),
        ])
    }

    /// Do the command `text` names, if it names one: the question on
    /// screen answered, a dialog's button, what is on top put away; a
    /// palette command; a setting's choice; a button, a choice or a list's
    /// row on screen; a phrase with a value. `None` leaves it to Ask.
    fn heard_command(&mut self, text: &str) -> Option<Task<Message>> {
        // What is on screen, as it is drawn now.
        let targets = crate::ui::heard::gather(self);
        // A dialog takes the voice before anything, as it takes the pointer.
        if let Some(task) = self.heard_on_screen(text, &targets, true) {
            return Some(task);
        }
        let commands = self.palette_commands();
        let titles: Vec<&str> = commands.iter().map(|c| c.title.as_str()).collect();
        // Said in English, or as the palette shows it in the reader's
        // language.
        let found = clew_voice::intent::command(text, &titles).or_else(|| {
            let shown: Vec<&str> = commands.iter().map(|c| c.shown()).collect();
            said_as_shown(crate::i18n::current(), text, &shown)
        });
        if let Some(i) = found {
            let command = &commands[i];
            let title = command.title.clone();
            // What the pill says it ran: the title the reader sees, made of
            // parts where the catalogs translate it so.
            let said = if command.shown_as.is_some() {
                command.shown().to_string()
            } else {
                title.clone()
            };
            if command.asks_when_said() {
                self.status = tr!(
                    "“{text}” → {command}?",
                    text = text,
                    command = crate::i18n::lookup(&said)
                );
                self.voice.confirm = Some(Waiting::Command(title));
                self.voice.hear(Heard {
                    text: text.to_string(),
                    outcome: HeardOutcome::Confirm(said),
                });
                return Some(Task::none());
            }
            self.status = tr!(
                "“{text}” → {command}",
                text = text,
                command = crate::i18n::lookup(&said)
            );
            self.voice.hear(Heard {
                text: text.to_string(),
                outcome: HeardOutcome::Ran(said),
            });
            return Some(self.run_palette(&title));
        }
        // A setting's choice, said as anyone says it: "change the app's
        // language to French".
        if let Some(task) = self.heard_choice(text, |_| true) {
            return Some(task);
        }
        // What is on screen, by its name or its place in a list.
        if let Some(task) = self.heard_on_screen(text, &targets, false) {
            return Some(task);
        }
        // A line, a file, a symbol, words to look for: said with what no
        // title names.
        if let Some((done, task)) = self.spoken(text) {
            return Some(self.did_as_said(text.to_string(), done, task));
        }
        None
    }

    /// Spoken commands turned off, what is said goes to Ask — but for
    /// turning them back on, which nothing else hands-free could do.
    fn heard_commands_back_on(&mut self, text: &str) -> Option<Task<Message>> {
        self.heard_choice(text, |m| {
            matches!(m, Message::Voice(VoiceMsg::SetRunCommands(true)))
        })
    }

    /// Make the setting's choice `text` asks for ([`crate::app::choices::
    /// chosen`]), if `wanted` takes it.
    fn heard_choice(
        &mut self,
        text: &str,
        wanted: impl Fn(&Message) -> bool,
    ) -> Option<Task<Message>> {
        let settings = self.settings_choices();
        let (s, c) = crate::app::choices::chosen(text, &settings)?;
        let choice = settings[s].choices[c].clone();
        if !wanted(&choice.message) {
            return None;
        }
        self.status = tr!("“{text}” → {command}", text = text, command = choice.shown);
        self.voice.hear(Heard {
            text: text.to_string(),
            outcome: HeardOutcome::Ran(choice.shown),
        });
        Some(self.update(choice.message))
    }

    /// `text` came to `done` ([`App::spoken`]): said on the status
    /// line and the pill, as a command run is — or, when it could not be
    /// done, as a failure is.
    fn did_as_said(
        &mut self,
        text: String,
        done: crate::app::spoken::Done,
        task: Task<Message>,
    ) -> Task<Message> {
        use crate::app::spoken::Done;
        match done {
            Done::Did(done) => {
                self.status = tr!("“{text}” → {command}", text = text, command = done);
                self.voice.hear(Heard {
                    text,
                    outcome: HeardOutcome::Ran(done),
                });
            }
            Done::Cannot(why) => {
                self.status = why.clone();
                self.voice.hear(Heard {
                    text,
                    outcome: HeardOutcome::Failed(why),
                });
            }
        }
        task
    }

    /// Download model `id` into the store, verified as it lands.
    fn start_model_download(&mut self, id: &'static str) -> Task<Message> {
        let Some(model) = models::find(id) else {
            return Task::none();
        };
        if self.voice.downloads.contains_key(id) {
            return Task::none();
        }
        let Some(store) = self.voice.store.clone() else {
            self.status = tr!("There is no data directory to keep the model in").into();
            return Task::none();
        };
        let claim = match Claim::take(store.path(model), ClaimedFor::Download) {
            Ok(claim) => claim,
            Err(held) => {
                self.status = held.busy(crate::i18n::lookup(model.name));
                return Task::none();
            }
        };
        self.voice.download_errors.remove(id);
        let cancel = Arc::new(AtomicBool::new(false));
        let fetch = self.voice.fetch;
        let stream = iced::stream::channel(64, {
            let cancel = cancel.clone();
            move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                use iced::futures::SinkExt;
                // Raised when nobody listens any more — the window closed
                // with the download running — so the transfer stops too.
                let _stop = StopOnDrop(cancel.clone());
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
                let worker_cancel = cancel.clone();
                tokio::task::spawn_blocking(move || {
                    let result = (|| {
                        let mut install = store
                            .begin(model)
                            .map_err(|e| tr!("could not save the model: {error}", error = e))?;
                        let mut last = Instant::now();
                        let mut progress = |done: u64, total: Option<u64>| {
                            if last.elapsed() >= Duration::from_millis(150) {
                                last = Instant::now();
                                let _ = tx.send(Message::Voice(VoiceMsg::DownloadProgress {
                                    id,
                                    done,
                                    total,
                                }));
                            }
                        };
                        match fetch(
                            &model.url(),
                            model.max_bytes(),
                            &worker_cancel,
                            &mut install,
                            &mut progress,
                        ) {
                            // Cancelled as it ended: not put in place — the
                            // model may be being removed by now.
                            Ok(_) if worker_cancel.load(Ordering::Relaxed) => {
                                Err(clew_core::llm::CANCELLED.to_string())
                            }
                            Ok(_) => install.finish(),
                            Err(e) => Err(download_failure(&e, install.written(), model)),
                        }
                    })();
                    let _ = tx.send(Message::Voice(VoiceMsg::Downloaded { id, result }));
                });
                while let Some(message) = rx.recv().await {
                    let done = matches!(message, Message::Voice(VoiceMsg::Downloaded { .. }));
                    if output.send(message).await.is_err() || done {
                        break;
                    }
                }
            }
        });
        let (task, handle) = Task::run(stream, |m| m).abortable();
        self.voice.downloads.insert(
            id,
            ModelDownload {
                done: 0,
                total: None,
                cancel,
                _claim: claim,
                _task: handle.abort_on_drop(),
            },
        );
        self.status = tr!(
            "Downloading {model} ({size} MB)…",
            model = crate::i18n::lookup(model.name),
            size = model.size_mb
        );
        task
    }
}

/// What a failed download of `model` says, under the model in Settings: why,
/// in the reader's language, with the address it failed at cut to its host
/// (a CDN's signed one is a paragraph), and how far it got if it got
/// anywhere.
pub(crate) fn download_failure(error: &str, written: u64, model: &models::Model) -> String {
    download_failure_sized(error, written, model.size_mb)
}

/// [`download_failure`], for a download of `size_mb` MB.
pub(crate) fn download_failure_sized(error: &str, written: u64, size_mb: u32) -> String {
    let why = if clew_core::net::was_cut_short(error) {
        tr!("the connection closed").to_string()
    } else {
        crate::i18n::message(&with_host_only(error))
    };
    if written == 0 {
        return why;
    }
    tr!(
        "Stopped at {done} of {total} MB: {why}",
        done = written / 1_000_000,
        total = size_mb,
        why = why
    )
}

/// A transfer's `error` with the address it starts with cut to its host
/// (and port, when it names one). Every error a transfer returns names the hop that failed first — `{url}:
/// why`, or `{url} (through the proxy {spec}): why` — and the catalogs say
/// it whole, address and all; what follows the address is left as it is,
/// for them to know it.
fn with_host_only(error: &str) -> String {
    let Some(rest) = ["https://", "http://"]
        .iter()
        .find_map(|scheme| error.strip_prefix(scheme))
    else {
        return error.to_string();
    };
    // An address holds no spaces: it is all up to the first one, less the
    // colon a reason follows it with.
    let (address, after) = rest.split_at(rest.find(' ').unwrap_or(rest.len()));
    let (address, colon) = match address.strip_suffix(':') {
        Some(address) => (address, ":"),
        None => (address, ""),
    };
    let authority = address.split(['/', '?', '#']).next().unwrap_or_default();
    // Never a user's name or password, should the address hold one.
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host.is_empty() {
        return error.to_string();
    }
    format!("{host}{colon}{after}")
}

/// The models being downloaded in this process, by the file each becomes:
/// two windows downloading one model wrote one partial file between them,
/// and the first to finish moved the other's half-written bytes into place.
static DOWNLOADING: LazyLock<Mutex<HashMap<PathBuf, ClaimedFor>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// What a model's files are claimed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimedFor {
    Download,
    Removal,
}

impl ClaimedFor {
    /// Why `model` cannot be downloaded or removed now: its files are
    /// claimed for this.
    pub(crate) fn busy(self, model: &str) -> String {
        match self {
            ClaimedFor::Download => tr!(
                "{model} is already downloading in another window",
                model = model
            ),
            ClaimedFor::Removal => tr!("{model} is being removed", model = model),
        }
    }
}

/// A model's files, claimed for the transfer that writes them or the
/// removal that deletes them — held until it ends, finished, failed or
/// stopped.
pub(crate) struct Claim(PathBuf);

impl Claim {
    /// Claim the files that go to `dest`, for `purpose`; while another
    /// holds them, what for.
    pub(crate) fn take(dest: PathBuf, purpose: ClaimedFor) -> Result<Claim, ClaimedFor> {
        {
            let mut claimed = DOWNLOADING.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(held) = claimed.get(&dest) {
                return Err(*held);
            }
            claimed.insert(dest.clone(), purpose);
        }
        // Made only when taken, and after the lock is let go: a claim
        // dropped under it locked it again, and gave up another's.
        Ok(Claim(dest))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        DOWNLOADING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// Whether the model that goes to `dest` is being downloaded — or removed
/// — in any window.
pub(crate) fn downloading(dest: &Path) -> bool {
    DOWNLOADING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(dest)
}

/// Sets its flag when dropped.
pub(crate) struct StopOnDrop(pub(crate) Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
