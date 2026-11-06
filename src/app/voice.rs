//! Voice: talking to the window instead of typing to it.
//!
//! The voice key (⇧⌘S, rebindable) or the microphone button starts
//! listening; pressing either again stops — as does a pause once something
//! has been said, so a spoken command needs no second key — and Esc
//! cancels. What was heard is transcribed on this machine, by whisper.cpp on
//! a model the user downloaded once ([`clew_voice::models`]): nothing that
//! is said leaves the computer.
//!
//! The words are then read as one of the window's commands when they
//! plainly name one ("open the call graph", "hide the minimap" —
//! [`clew_voice::intent`]), which runs as if picked from the command
//! palette; a command that costs something (a paid pass, starting the
//! program) asks first. Anything else is a question, and goes to Ask.
//!
//! Its messages, [`VoiceMsg`], arrive through `App::update_voice`.

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
/// How long listening waits for anything to be said before giving up.
pub(crate) const NO_SPEECH_TIMEOUT: f32 = 8.0;
/// A second press of the voice key sooner than this after the last one is
/// the key repeating while held — or bouncing — not a press: ignored.
pub(crate) const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(400);
/// How often the level meter and the pause are read while listening.
pub(crate) const LISTEN_TICK: Duration = Duration::from_millis(60);

/// The microphone while it listens: the system's ([`Recorder`]), or a
/// test's.
pub(crate) trait Listening: Send {
    /// How loud the latest moment was, 0 to 1.
    fn level(&self) -> f32;
    /// Seconds of quiet since the speaker last spoke; `None` before any
    /// speech.
    fn silence_after_speech(&self) -> Option<f32>;
    /// Whether it has heard all it may keep.
    fn full(&self) -> bool;
    /// A failure since it started (the device went away).
    fn error(&self) -> Option<String>;
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
    fn full(&self) -> bool {
        Recorder::full(self)
    }
    fn error(&self) -> Option<String> {
        Recorder::error(self)
    }
    fn stop(self: Box<Self>) -> Recording {
        Recorder::stop(*self)
    }
}

/// Opens the microphone: the system's, or a test's.
pub(crate) type OpenMic = fn(u32) -> Result<Box<dyn Listening>, String>;

fn open_system_mic(max_seconds: u32) -> Result<Box<dyn Listening>, String> {
    Recorder::start(max_seconds).map(|r| Box::new(r) as Box<dyn Listening>)
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

/// What voice is doing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum VoicePhase {
    Idle,
    /// The microphone is open.
    Listening {
        since: Instant,
        /// The meter's latest reading, 0 to 1.
        level: f32,
    },
    /// What was heard is being turned into words; `token` names the run.
    Transcribing {
        token: u64,
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
    /// Nothing was heard that could be read.
    Nothing,
    /// It failed, saying why.
    Failed(String),
}

/// A model being downloaded.
pub(crate) struct ModelDownload {
    pub done: u64,
    pub total: Option<u64>,
    cancel: Arc<AtomicBool>,
    /// Aborts the transfer when dropped — when the window closes with it
    /// running.
    _task: iced::task::Handle,
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
}

impl Default for VoicePrefs {
    fn default() -> Self {
        Self {
            model: models::DEFAULT_MODEL.to_string(),
            language: None,
            run_commands: true,
            send_questions: true,
        }
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
        prefs
    }

    fn save(&self) -> Result<(), String> {
        let mut section = toml::Table::new();
        section.insert("model".into(), self.model.clone().into());
        if let Some(language) = &self.language {
            section.insert("language".into(), language.clone().into());
        }
        section.insert("commands".into(), self.run_commands.into());
        section.insert("send".into(), self.send_questions.into());
        clew_core::globalconfig::update("voice", section)
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
    /// The microphone, while listening.
    listening: Option<Box<dyn Listening>>,
    /// Stops the transcription in progress.
    cancel: Arc<AtomicBool>,
    /// The last run started, so a late result of an earlier one is dropped.
    token: u64,
    /// When the voice key last started or stopped listening.
    last_toggle: Option<Instant>,
    /// What was last heard, and what came of it.
    pub heard: Option<Heard>,
    /// When it was heard: the pill in the corner says it for a moment
    /// ([`HEARD_SHOWN`]).
    pub heard_at: Option<Instant>,
    /// A command heard that asks before it runs: its title.
    pub confirm: Option<String>,
    /// Models downloading, by id.
    pub downloads: HashMap<&'static str, ModelDownload>,
    /// The last download's failure, by model id.
    pub download_errors: HashMap<&'static str, String>,
    pub prefs: VoicePrefs,
    /// Keep listening: once Clew has answered, the microphone opens again
    /// for what is said next — a conversation by voice, the card open.
    pub keep_listening: bool,
    /// Where models are kept: under the data directory.
    pub store: Option<Store>,
    pub(crate) open_mic: OpenMic,
    pub(crate) transcriber: Arc<dyn Transcriber>,
    pub(crate) fetch: Fetch,
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
            cancel: Arc::new(AtomicBool::new(false)),
            token: 0,
            last_toggle: None,
            heard: None,
            heard_at: None,
            confirm: None,
            downloads: HashMap::new(),
            download_errors: HashMap::new(),
            prefs: VoicePrefs::load(),
            keep_listening: false,
            store: clew_core::lsp::store::data_root().map(|d| Store::new(d.join("voice"))),
            open_mic: open_system_mic,
            transcriber: ENGINE.clone(),
            fetch: fetch_over_net,
        }
    }

    /// Whether Esc has something of voice's to put away: listening, a
    /// transcription, a command waiting for a yes.
    pub(crate) fn is_busy(&self) -> bool {
        self.phase != VoicePhase::Idle || self.confirm.is_some()
    }

    pub(crate) fn is_listening(&self) -> bool {
        matches!(self.phase, VoicePhase::Listening { .. })
    }

    /// Back to idle, as before anything was said — the key's last press
    /// forgotten too, so a test's next press is not taken for a repeat.
    #[cfg(test)]
    pub(crate) fn reset_for_test(&mut self) {
        self.phase = VoicePhase::Idle;
        self.listening = None;
        self.confirm = None;
        self.heard = None;
        self.last_toggle = None;
    }

    /// Whether `model` is downloaded.
    pub(crate) fn installed(&self, model: &Model) -> bool {
        self.store.as_ref().is_some_and(|s| s.installed(model))
    }
}

/// Voice's messages.
#[derive(Debug, Clone)]
pub enum VoiceMsg {
    /// The voice key or the microphone button: start listening, or stop and
    /// transcribe.
    Toggle,
    /// Stop listening and transcribe what was heard.
    Stop,
    /// Esc: stop listening, or stop transcribing, keeping nothing; or say no
    /// to the command waiting for a yes.
    Cancel,
    /// While listening: read the level and look for the pause that ends it.
    Tick,
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
    /// Listen again after each answer, or not.
    SetKeepListening(bool),
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
    // "silvousplaît". Longest first, so the shorter does not cut it short.
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
    let bare = |text: &str| -> String {
        let mut text = compact(text);
        loop {
            let before = text.len();
            for word in leading {
                if let Some(rest) = text.strip_prefix(word)
                    && !rest.is_empty()
                {
                    text = rest.to_string();
                }
            }
            for word in trailing {
                if let Some(rest) = text.strip_suffix(word)
                    && !rest.is_empty()
                {
                    text = rest.to_string();
                }
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
    let mut found = None;
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
        let fits = [*title, main, &plain(title), &plain(main)]
            .iter()
            .any(|form| bare(form) == said);
        if fits {
            if found.is_some() {
                return None;
            }
            found = Some(index);
        }
    }
    found
}

impl App {
    /// Handle a [`VoiceMsg`].
    pub(crate) fn update_voice(&mut self, message: VoiceMsg) -> Task<Message> {
        match message {
            VoiceMsg::Toggle => self.on_voice_toggle(),
            VoiceMsg::SetKeepListening(on) => {
                self.voice.keep_listening = on;
                Task::none()
            }
            VoiceMsg::Stop => self.stop_listening(),
            VoiceMsg::Cancel => {
                self.cancel_voice();
                Task::none()
            }
            VoiceMsg::Tick => self.on_voice_tick(),
            VoiceMsg::Transcribed { token, result } => self.on_transcribed(token, result),
            VoiceMsg::Confirm => match self.voice.confirm.take() {
                Some(title) => {
                    self.voice.heard = self.voice.heard.take().map(|h| Heard {
                        outcome: HeardOutcome::Ran(title.clone()),
                        ..h
                    });
                    self.run_palette(&title)
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
                        self.status = tr!(
                            "{model} is ready — press the voice key to talk",
                            model = name
                        );
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
                if let (Some(model), Some(store)) = (models::find(id), self.voice.store.as_ref()) {
                    let name = crate::i18n::lookup(model.name);
                    self.status = match store.remove(model) {
                        Ok(()) => tr!("Removed {model}", model = name),
                        Err(e) => tr!("Could not remove {model}: {error}", model = name, error = e),
                    };
                }
                Task::none()
            }
            VoiceMsg::PickModel(id) => {
                if models::find(id).is_some() {
                    self.voice.prefs.model = id.to_string();
                    self.save_voice_prefs();
                }
                Task::none()
            }
            VoiceMsg::SetLanguage(language) => {
                self.voice.prefs.language = language.map(str::to_string);
                self.save_voice_prefs();
                Task::none()
            }
            VoiceMsg::SetRunCommands(on) => {
                self.voice.prefs.run_commands = on;
                self.save_voice_prefs();
                Task::none()
            }
            VoiceMsg::SetSendQuestions(on) => {
                self.voice.prefs.send_questions = on;
                self.save_voice_prefs();
                Task::none()
            }
        }
    }

    fn save_voice_prefs(&mut self) {
        if let Err(e) = self.voice.prefs.save() {
            self.status = tr!("Could not save the voice settings: {error}", error = e);
        }
    }

    /// The voice key or the microphone button.
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
        match self.voice.phase {
            VoicePhase::Listening { .. } => self.stop_listening(),
            VoicePhase::Transcribing { .. } => {
                self.status = tr!("Still working out what was said…").into();
                Task::none()
            }
            VoicePhase::Idle => self.start_listening(),
        }
    }

    /// After an answer, with Keep listening on: the microphone opens again
    /// for what is said next — while the conversation is open, nothing is
    /// heard or waiting for a yes, and there is a model to hear it with
    /// (this never sends the reader to Settings for one).
    pub(crate) fn resume_listening(&mut self) -> Task<Message> {
        let ready = self.voice.keep_listening
            && self.ask_view.is_open()
            && self.voice.phase == VoicePhase::Idle
            && self.voice.confirm.is_none()
            && self.voice.installed(self.voice.prefs.model());
        if ready {
            self.start_listening()
        } else {
            Task::none()
        }
    }

    /// Open the microphone — once there is a model to hear it with.
    fn start_listening(&mut self) -> Task<Message> {
        self.voice.confirm = None;
        let model = self.voice.prefs.model();
        if !self.voice.installed(model) {
            self.status = tr!(
                "Voice needs a speech model on this computer — download {model} ({size} MB) \
                 in Settings → Voice",
                model = crate::i18n::lookup(model.name),
                size = model.size_mb
            );
            return self.update(Message::Settings(SettingsMsg::OpenAt(SettingsPage::Voice)));
        }
        match (self.voice.open_mic)(MAX_SECONDS) {
            Ok(listening) => {
                self.voice.listening = Some(listening);
                self.voice.phase = VoicePhase::Listening {
                    since: Instant::now(),
                    level: 0.0,
                };
                self.voice.heard = None;
                self.status = tr!("Listening… say a command or a question (Esc cancels)").into();
            }
            Err(e) => {
                self.status = tr!("Can't listen: {error}", error = e);
                self.voice.hear(Heard {
                    text: String::new(),
                    outcome: HeardOutcome::Failed(e),
                });
            }
        }
        Task::none()
    }

    /// While listening: the meter, and whether it is time to stop.
    fn on_voice_tick(&mut self) -> Task<Message> {
        let VoicePhase::Listening { since, .. } = self.voice.phase else {
            return Task::none();
        };
        let Some(listening) = self.voice.listening.as_ref() else {
            return Task::none();
        };
        if let Some(e) = listening.error() {
            self.cancel_voice();
            self.status = tr!("Stopped listening: {error}", error = e);
            return Task::none();
        }
        let level = listening.level();
        self.voice.phase = VoicePhase::Listening { since, level };
        let ended = listening.full()
            || listening
                .silence_after_speech()
                .is_some_and(|quiet| quiet >= END_SILENCE);
        if ended {
            return self.stop_listening();
        }
        if listening.silence_after_speech().is_none()
            && since.elapsed().as_secs_f32() >= NO_SPEECH_TIMEOUT
        {
            self.cancel_voice();
            self.voice.hear(Heard {
                text: String::new(),
                outcome: HeardOutcome::Nothing,
            });
            self.status = tr!("Nothing was heard — check the microphone and try again").into();
        }
        Task::none()
    }

    /// Stop listening and turn what was heard into words, off the UI
    /// thread.
    fn stop_listening(&mut self) -> Task<Message> {
        let Some(listening) = self.voice.listening.take() else {
            return Task::none();
        };
        let recording = listening.stop();
        self.voice.token += 1;
        let token = self.voice.token;
        self.voice.phase = VoicePhase::Transcribing { token };
        self.voice.cancel = Arc::new(AtomicBool::new(false));
        self.status = tr!("Transcribing…").into();
        let model = self.voice.prefs.model();
        let Some(path) = self.voice.store.as_ref().map(|s| s.path(model)) else {
            return self.on_transcribed(token, Err(tr!("no data directory for the model").into()));
        };
        // The window's command names, so whisper spells them as they are
        // spelled here.
        let hint = self
            .palette_commands()
            .iter()
            .map(|c| c.shown().trim_end_matches('…').to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let options = Options {
            language: self.voice.prefs.language.clone(),
            hint,
            threads: 0,
        };
        let cancel = self.voice.cancel.clone();
        let transcriber = self.voice.transcriber.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    match clew_voice::audio::prepare(&recording.samples, recording.rate) {
                        // Nothing that sounded like speech: whisper is not
                        // asked to make words of the room.
                        None => Ok(String::new()),
                        Some(audio) => {
                            transcriber.transcribe(&path, audio.as_slice(), &options, &cancel)
                        }
                    }
                })
                .await
                .map_err(|_| tr!("transcribing failed unexpectedly").to_string())?
            },
            move |result| Message::Voice(VoiceMsg::Transcribed { token, result }),
        )
    }

    /// Esc: stop listening or transcribing and keep nothing; or say no to
    /// the command that waits for a yes.
    pub(crate) fn cancel_voice(&mut self) {
        if let Some(listening) = self.voice.listening.take() {
            drop(listening.stop());
        }
        if matches!(self.voice.phase, VoicePhase::Transcribing { .. }) {
            self.voice.cancel.store(true, Ordering::Relaxed);
        }
        if self.voice.phase != VoicePhase::Idle {
            self.status = tr!("Stopped listening").into();
        }
        self.voice.phase = VoicePhase::Idle;
        if self.voice.confirm.take().is_some() {
            self.voice.heard = None;
        }
    }

    /// The words heard: a command, run (or asked about); else a question
    /// for Ask.
    fn on_transcribed(&mut self, token: u64, result: Result<String, String>) -> Task<Message> {
        if token != self.voice.token || self.voice.phase != (VoicePhase::Transcribing { token }) {
            return Task::none();
        }
        self.voice.phase = VoicePhase::Idle;
        let text = match result {
            Ok(text) => text.trim().to_string(),
            Err(e) if e == "cancelled" => return Task::none(),
            Err(e) => {
                self.status = tr!("Could not transcribe: {error}", error = e);
                self.voice.hear(Heard {
                    text: String::new(),
                    outcome: HeardOutcome::Failed(e),
                });
                return Task::none();
            }
        };
        if text.is_empty() {
            self.status = tr!("Didn't catch that — try again").into();
            self.voice.hear(Heard {
                text,
                outcome: HeardOutcome::Nothing,
            });
            return Task::none();
        }
        if self.voice.prefs.run_commands {
            let commands = self.palette_commands();
            let titles: Vec<&str> = commands.iter().map(|c| c.title.as_str()).collect();
            // Said in English, or as the palette shows it in the reader's
            // language.
            let found = clew_voice::intent::command(&text, &titles).or_else(|| {
                let shown: Vec<&str> = commands.iter().map(|c| c.shown()).collect();
                said_as_shown(crate::i18n::current(), &text, &shown)
            });
            if let Some(i) = found {
                let command = &commands[i];
                let title = command.title.clone();
                if command.costly() {
                    self.status = tr!(
                        "“{text}” → {command}? Confirm or press Esc",
                        text = text,
                        command = crate::i18n::lookup(&title)
                    );
                    self.voice.confirm = Some(title.clone());
                    self.voice.hear(Heard {
                        text,
                        outcome: HeardOutcome::Confirm(title),
                    });
                    return Task::none();
                }
                self.status = tr!(
                    "“{text}” → {command}",
                    text = text,
                    command = crate::i18n::lookup(&title)
                );
                self.voice.hear(Heard {
                    text,
                    outcome: HeardOutcome::Ran(title.clone()),
                });
                return self.run_palette(&title);
            }
        }
        // A question: to Ask, with the window open on it.
        self.voice.hear(Heard {
            text: text.clone(),
            outcome: HeardOutcome::Asked,
        });
        self.reveal_ask();
        let mut input = self.proj.ask_input.trim_end().to_string();
        if !input.is_empty() {
            input.push(' ');
        }
        input.push_str(&text);
        self.proj.ask_input = input;
        self.proj.ask_spoken = true;
        if self.voice.prefs.send_questions && !self.ask_stream_active() {
            self.status = tr!("Asking “{text}”", text = text);
            return self.update(Message::Ask(AskMsg::Submit));
        }
        self.code_focused = false;
        Task::batch([
            operation::focus(ui::ask_input_id()),
            operation::move_cursor_to_end(ui::ask_input_id()),
        ])
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
/// without the address it failed at (a CDN's signed one is a paragraph), and
/// how far it got if it got anywhere.
pub(crate) fn download_failure(error: &str, written: u64, model: &models::Model) -> String {
    let why = if clew_core::net::was_cut_short(error) {
        tr!("the connection closed").to_string()
    } else {
        clew_core::net::without_url(error)
    };
    if written == 0 {
        return why;
    }
    tr!(
        "Stopped at {done} of {total} MB: {why}",
        done = written / 1_000_000,
        total = model.size_mb,
        why = why
    )
}

/// Sets its flag when dropped.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
