//! Speech (`app::speech`): answers to spoken questions read aloud — by a
//! test's voice, which says what it is asked to as a few samples, onto a
//! test's output, which keeps what it is given — and stopped when the
//! reader asks; Keep listening waiting for Clew to finish; the speech
//! settings and models.

use super::*;
use crate::app::speech::{Playing, Synthesizer};
use crate::app::voice::{Claim, ClaimedFor, VoicePhase, VoicePrefs};
use clew_protocol::StreamOutcome;
use clew_voice::speaker::SayError;
use clew_voice::speech::{self, VoiceModel};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// One sentence the test's voice was asked to say.
#[derive(Debug, Clone, PartialEq)]
struct Said {
    model: &'static str,
    text: String,
    lang: String,
    voice: i32,
    speed: f32,
}

/// A voice that says anything as a tenth of a second per word — or fails as
/// the test says — and keeps what it was asked.
#[derive(Default)]
struct Voice {
    said: Mutex<Vec<Said>>,
    /// The failure of what it is asked next, once.
    fail: Mutex<Option<SayError>>,
    /// The rate it speaks at; 1 kHz until a test says otherwise.
    rate: Mutex<u32>,
    stopped: AtomicUsize,
}

impl Synthesizer for Voice {
    fn say(
        &self,
        model: &'static VoiceModel,
        _dir: &Path,
        text: &str,
        lang: &str,
        voice: i32,
        speed: f32,
    ) -> Result<(Vec<f32>, u32), SayError> {
        if let Some(e) = self.fail.lock().unwrap().take() {
            return Err(e);
        }
        self.said.lock().unwrap().push(Said {
            model: model.id,
            text: text.into(),
            lang: lang.into(),
            voice,
            speed,
        });
        let words = text.split_whitespace().count().max(1);
        let rate = match *self.rate.lock().unwrap() {
            0 => 1000,
            rate => rate,
        };
        Ok((vec![0.25; words * 100 * rate as usize / 1000], rate))
    }
    fn stop_while(&self, _model: &'static VoiceModel, work: &mut dyn FnMut()) {
        self.stopped.fetch_add(1, Ordering::Relaxed);
        work();
    }
}

impl Voice {
    fn texts(&self) -> Vec<String> {
        self.said
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.text.clone())
            .collect()
    }
}

/// What the test's output was given, and how it says it is doing.
#[derive(Default)]
struct OutputScript {
    /// Samples given, sentence by sentence.
    played: Vec<usize>,
    /// The rate each was given at.
    rates: Vec<u32>,
    /// Samples it has played: the test says.
    progress: u64,
    /// What it was given that it has not played: taken by `take_rest`.
    rest: Vec<f32>,
    /// Whether all it was given has been played: the test says.
    idle: bool,
    error: Option<String>,
    opened: usize,
    /// There is no speaker to open.
    absent: bool,
}

struct Output(Arc<Mutex<OutputScript>>);

impl Playing for Output {
    fn play(&self, samples: &[f32], rate: u32) {
        let mut script = self.0.lock().unwrap();
        script.played.push(samples.len());
        script.rates.push(rate);
        script.rest.extend_from_slice(samples);
        script.idle = false;
    }
    fn played(&self) -> u64 {
        self.0.lock().unwrap().progress
    }
    fn take_rest(&self) -> (Vec<f32>, u32) {
        (std::mem::take(&mut self.0.lock().unwrap().rest), 1000)
    }
    fn rate(&self) -> u32 {
        1000
    }
    fn queued_seconds(&self) -> f32 {
        0.0
    }
    fn idle(&self) -> bool {
        self.0.lock().unwrap().idle
    }
    fn level(&self) -> f32 {
        0.4
    }
    fn error(&self) -> Option<String> {
        self.0.lock().unwrap().error.clone()
    }
}

/// Opens the test's output — or, `absent`, finds no speaker.
fn fake_output(script: Arc<Mutex<OutputScript>>) -> crate::app::speech::OpenOutput {
    Arc::new(move || {
        let mut s = script.lock().unwrap();
        if s.absent {
            return Err("no speaker or headphones are available".to_string());
        }
        s.opened += 1;
        Ok(Box::new(Output(script.clone())) as Box<dyn Playing>)
    })
}

/// Put `model`'s files where it is looked for: it is "downloaded".
fn install(app: &App, model: &VoiceModel) {
    let store = app.voice.store.clone().unwrap();
    for file in model.files {
        let path = speech::path(&store, model, file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }
}

/// An app with a models folder of its own, Supertonic "downloaded", the
/// test's voice and output.
fn speech_app(tag: &str) -> (App, Arc<Voice>, Arc<Mutex<OutputScript>>) {
    let mut app = scanned_app(tag);
    let dir = test_dir(&format!("{tag}-models"));
    let _ = std::fs::remove_dir_all(&dir);
    app.voice.store = Some(clew_voice::models::Store::new(dir));
    app.voice.prefs = VoicePrefs::default();
    install(&app, speech::find("supertonic-3").unwrap());
    let voice = Arc::new(Voice::default());
    app.voice.speech.synthesizer = voice.clone();
    app.voice.speech.stall = (
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(300),
    );
    let output = Arc::new(Mutex::new(OutputScript::default()));
    app.voice.speech.open_output = fake_output(output.clone());
    (app, voice, output)
}

/// A question asked by voice (`spoken`) or typed, its answer about to
/// stream on `stream`.
fn asked(app: &mut App, stream: u64, spoken: bool) {
    app.proj.ask_turns.push(crate::app::model::AskTurn {
        stream,
        question: "what does total do?".into(),
        spoken,
        streaming: true,
        ..Default::default()
    });
}

/// Feed `task`'s messages back, and theirs, until there are none.
fn settle(app: &mut App, task: Task<Message>) {
    let mut pending = run_task(task);
    while !pending.is_empty() {
        let mut next = Vec::new();
        for message in pending {
            next.extend(run_task(app.update(message)));
        }
        pending = next;
    }
}

fn delta(app: &mut App, stream: u64, text: &str) {
    let task = app.on_ask_delta(stream, text.into());
    settle(app, task);
}

fn ended(app: &mut App, stream: u64, outcome: StreamOutcome) {
    let task = app.on_ask_stream_ended(stream, outcome);
    settle(app, task);
}

/// Wait out the tail the output is given once all is played, then tick.
fn all_played(app: &mut App, output: &Arc<Mutex<OutputScript>>) {
    output.lock().unwrap().idle = true;
    let task = app.update(Message::Voice(VoiceMsg::SpeakTick));
    settle(app, task);
    std::thread::sleep(std::time::Duration::from_millis(400));
    let task = app.update(Message::Voice(VoiceMsg::SpeakTick));
    settle(app, task);
}

/// An answer to a question that was said is said too, a sentence at a time
/// as it streams in — the last when the answer is over — in the reader's
/// voice and pace; code is said as words, a code block not at all. A typed
/// question's answer is not said, nor any with reading aloud turned off.
#[test]
fn a_spoken_answer_is_read_aloud_as_it_comes() {
    let (mut app, voice, output) = speech_app("speech-reads");
    app.voice.prefs.speed = 1.25;
    app.voice.prefs.voices.insert("supertonic-3".into(), 6);
    asked(&mut app, 1, true);
    delta(&mut app, 1, "`total()` adds every price.");
    assert!(voice.texts().is_empty(), "the sentence may still go on");
    delta(&mut app, 1, " Then it");
    assert_eq!(voice.texts(), ["total adds every price."]);
    assert_eq!(output.lock().unwrap().played, [400]);
    assert!(app.voice.speech.is_speaking());
    assert!(app.voice.is_busy(), "Esc has it to stop");
    delta(&mut app, 1, " rounds.\n```python\nprint('not said')\n```\n");
    assert_eq!(voice.texts().len(), 1);
    ended(&mut app, 1, StreamOutcome::Done);
    assert_eq!(
        voice.texts(),
        ["total adds every price.", "Then it rounds."]
    );
    let said = voice.said.lock().unwrap().clone();
    assert!(
        said.iter().all(|s| s.model == "supertonic-3"
            && s.lang == "en"
            && s.voice == 6
            && s.speed == 1.25),
        "{said:?}"
    );
    assert_eq!(
        output.lock().unwrap().opened,
        1,
        "one output for the answer"
    );
    // Played, it closes.
    all_played(&mut app, &output);
    assert!(!app.voice.speech.is_speaking());
    assert!(!app.voice.is_busy());
    // Typed: not said.
    asked(&mut app, 2, false);
    delta(&mut app, 2, "The total adds them. Then");
    ended(&mut app, 2, StreamOutcome::Done);
    assert_eq!(voice.texts().len(), 2);
    // Turned off: not said.
    let _ = app.update(Message::Voice(VoiceMsg::SetSpeak(false)));
    asked(&mut app, 3, true);
    delta(&mut app, 3, "The total adds them. Then");
    ended(&mut app, 3, StreamOutcome::Done);
    assert_eq!(voice.texts().len(), 2);
}

/// Each sentence is said in its own language: French as French, Korean by
/// its script; Chinese by Kokoro, once it is here — until then not said,
/// and the reader told where to get it, once.
#[test]
fn each_sentence_is_said_in_its_language() {
    let (mut app, voice, _) = speech_app("speech-languages");
    asked(&mut app, 1, true);
    delta(
        &mut app,
        1,
        "La fonction calcule le total de chaque article. 합계를 구합니다. 计算总价。",
    );
    ended(&mut app, 1, StreamOutcome::Done);
    let said = voice.said.lock().unwrap().clone();
    assert_eq!(
        said.iter()
            .map(|s| (s.lang.as_str(), s.model))
            .collect::<Vec<_>>(),
        [("fr", "supertonic-3"), ("ko", "supertonic-3")]
    );
    assert!(app.status.contains("Kokoro (Chinese)"), "{}", app.status);
    // Said once a window.
    app.status.clear();
    asked(&mut app, 2, true);
    delta(&mut app, 2, "计算总价。");
    ended(&mut app, 2, StreamOutcome::Done);
    assert!(!app.status.contains("Kokoro"), "{}", app.status);
    // Here: said, in its own voice.
    install(&app, speech::find("kokoro-zh").unwrap());
    asked(&mut app, 3, true);
    delta(&mut app, 3, "计算总价。");
    ended(&mut app, 3, StreamOutcome::Done);
    let last = voice.said.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        (last.model, last.lang.as_str(), last.voice),
        ("kokoro-zh", "zh", 47)
    );
}

/// Esc, Stop, the button on the pill: Clew stops speaking, and says no
/// more of that answer as more of it comes. The voice key stops it and
/// listens. A new question, Clear, closing Ask: it stops too.
#[test]
fn speech_stops_when_the_reader_asks() {
    let (mut app, voice, _) = speech_app("speech-stops");
    // Esc.
    asked(&mut app, 1, true);
    delta(&mut app, 1, "The total adds every price. Then");
    assert!(app.voice.speech.is_speaking());
    let _ = app.update(key_press(
        keyboard::Key::Named(keyboard::key::Named::Escape),
        keyboard::Modifiers::empty(),
    ));
    assert!(!app.voice.speech.is_speaking());
    assert_eq!(app.status, "Stopped speaking");
    delta(&mut app, 1, " it rounds. And it returns.");
    ended(&mut app, 1, StreamOutcome::Done);
    assert_eq!(voice.texts().len(), 1, "no more of a hushed answer");
    // The pill's Stop.
    asked(&mut app, 2, true);
    delta(&mut app, 2, "The total adds every price. Then");
    let _ = app.update(Message::Voice(VoiceMsg::StopSpeaking));
    assert!(!app.voice.speech.is_speaking());
    // Ask's Stop, while the answer still comes.
    asked(&mut app, 3, true);
    delta(&mut app, 3, "The total adds every price. Then");
    assert!(app.voice.speech.is_speaking());
    ended(&mut app, 3, StreamOutcome::Stopped);
    assert!(!app.voice.speech.is_speaking());
    // The voice key: said over Clew, it stops and listens.
    app.voice.open_mic = |_| Err("no microphone is available".into());
    let model = app.voice.prefs.model();
    let store = app.voice.store.clone().unwrap();
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.path(model), b"a model").unwrap();
    asked(&mut app, 4, true);
    delta(&mut app, 4, "The total adds every price. Then");
    assert!(app.voice.speech.is_speaking());
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(!app.voice.speech.is_speaking());
    assert!(
        app.status.contains("no microphone"),
        "it tried to listen: {}",
        app.status
    );
    // Closing Ask.
    asked(&mut app, 5, true);
    delta(&mut app, 5, "The total adds every price. Then");
    let _ = app.update(Message::Ask(AskMsg::Close));
    assert!(!app.voice.speech.is_speaking());
    // Clear.
    asked(&mut app, 6, true);
    delta(&mut app, 6, "The total adds every price. Then");
    let _ = app.update(Message::Ask(AskMsg::Clear));
    assert!(!app.voice.speech.is_speaking());
    // A sentence made after a stop is dropped when it comes: nothing plays.
    asked(&mut app, 7, true);
    let task = app.on_ask_delta(7, "The total adds every price. Then".into());
    let _ = app.update(Message::Voice(VoiceMsg::StopSpeaking));
    let before = voice.texts().len();
    settle(&mut app, task);
    assert_eq!(voice.texts().len(), before + 1, "it was made");
    assert!(!app.voice.speech.is_speaking(), "but not played");
}

/// Keep listening: the microphone opens once Clew has said all of the
/// answer — not while it speaks, which it would hear.
#[test]
fn keep_listening_waits_for_clew_to_finish() {
    let (mut app, _voice, output) = speech_app("speech-keep-listening");
    let model = app.voice.prefs.model();
    let store = app.voice.store.clone().unwrap();
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.path(model), b"a model").unwrap();
    app.voice.open_mic = |_| Ok(Box::new(Quiet) as Box<dyn crate::app::voice::Listening>);
    app.ask_view = AskView::Compact;
    let _ = app.update(Message::Voice(VoiceMsg::SetKeepListening(true)));
    asked(&mut app, 1, true);
    delta(&mut app, 1, "The total adds every price.");
    ended(&mut app, 1, StreamOutcome::Done);
    assert!(app.voice.speech.is_speaking());
    assert_eq!(app.voice.phase, VoicePhase::Idle, "not while Clew speaks");
    all_played(&mut app, &output);
    assert!(!app.voice.speech.is_speaking());
    assert!(
        matches!(app.voice.phase, VoicePhase::Listening { .. }),
        "{:?}",
        app.voice.phase
    );
    // Stopped by the reader: it does not listen on its own.
    let _ = app.update(Message::Voice(VoiceMsg::Cancel));
    asked(&mut app, 2, true);
    delta(&mut app, 2, "The total adds every price.");
    ended(&mut app, 2, StreamOutcome::Done);
    let _ = app.update(Message::Voice(VoiceMsg::StopSpeaking));
    all_played(&mut app, &output);
    assert_eq!(app.voice.phase, VoicePhase::Idle);
    // Nothing to say (all code): it listens at once.
    asked(&mut app, 3, true);
    delta(&mut app, 3, "```\nx = 1\n```");
    ended(&mut app, 3, StreamOutcome::Done);
    assert!(matches!(app.voice.phase, VoicePhase::Listening { .. }));
}

/// A microphone that hears nothing.
struct Quiet;

impl crate::app::voice::Listening for Quiet {
    fn level(&self) -> f32 {
        0.0
    }
    fn silence_after_speech(&self) -> Option<f32> {
        None
    }
    fn full(&self) -> bool {
        false
    }
    fn error(&self) -> Option<String> {
        None
    }
    fn stop(self: Box<Self>) -> clew_voice::capture::Recording {
        clew_voice::capture::Recording {
            samples: Vec::new(),
            rate: 16_000,
        }
    }
}

/// A sentence that cannot be said is passed over; a voice that cannot
/// speak at all — not installed, ended — stops speaking and says why; so
/// does an output that cannot open, or goes away.
#[test]
fn what_cannot_be_said_says_why() {
    let (mut app, voice, output) = speech_app("speech-fails");
    *voice.fail.lock().unwrap() = Some(SayError::Sentence("nothing could be made of it".into()));
    asked(&mut app, 1, true);
    delta(&mut app, 1, "First one. Second one. Third");
    ended(&mut app, 1, StreamOutcome::Done);
    assert_eq!(voice.texts(), ["Second one.", "Third"], "the next was said");
    all_played(&mut app, &output);
    *voice.fail.lock().unwrap() = Some(SayError::Voice(
        "clew-speak, which speaks for Clew, is not installed beside it".into(),
    ));
    asked(&mut app, 2, true);
    delta(&mut app, 2, "First one. Second one. Third");
    ended(&mut app, 2, StreamOutcome::Done);
    assert!(!app.voice.speech.is_speaking());
    assert!(
        app.status.starts_with("Could not speak: clew-speak"),
        "{}",
        app.status
    );
    assert_eq!(voice.texts().len(), 2, "nothing more was asked of it");
    // No output.
    output.lock().unwrap().absent = true;
    asked(&mut app, 3, true);
    delta(&mut app, 3, "First one. Second");
    assert!(
        app.status.contains("no speaker or headphones"),
        "{}",
        app.status
    );
    assert!(!app.voice.speech.is_speaking());
    // A failure it plays through is no stop.
    output.lock().unwrap().absent = false;
    output.lock().unwrap().rest.clear();
    asked(&mut app, 4, true);
    delta(&mut app, 4, "First one. Second");
    output.lock().unwrap().error = Some("the speaker failed: an underrun".into());
    let tick = |app: &mut App| {
        let task = app.update(Message::Voice(VoiceMsg::SpeakTick));
        settle(app, task);
    };
    for _ in 0..4 {
        output.lock().unwrap().progress += 100;
        tick(&mut app);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    assert!(app.voice.speech.is_speaking(), "{}", app.status);
    // One that plays no more is opened again, with what it had left …
    let opened = output.lock().unwrap().opened;
    tick(&mut app);
    std::thread::sleep(std::time::Duration::from_millis(80));
    tick(&mut app);
    assert!(app.voice.speech.is_speaking(), "{}", app.status);
    assert_eq!(output.lock().unwrap().opened, opened + 1, "opened again");
    assert_eq!(
        output.lock().unwrap().played.last(),
        Some(&200),
        "with the rest of what it had"
    );
    // … and when that one plays nothing either, Clew stops, saying why.
    tick(&mut app);
    std::thread::sleep(std::time::Duration::from_millis(80));
    tick(&mut app);
    assert!(!app.voice.speech.is_speaking());
    assert!(app.status.contains("an underrun"), "{}", app.status);
    // One that plays nothing, and says nothing, is taken for stopped too.
    output.lock().unwrap().error = None;
    asked(&mut app, 5, true);
    delta(&mut app, 5, "First one. Second");
    for _ in 0..2 {
        tick(&mut app);
        std::thread::sleep(std::time::Duration::from_millis(350));
        tick(&mut app);
    }
    assert!(!app.voice.speech.is_speaking());
    assert!(
        app.status.contains("the speaker stopped playing"),
        "{}",
        app.status
    );
    // Without the voice downloaded: nothing said, and where to get it.
    let (mut app, voice, _) = speech_app("speech-none");
    let store = app.voice.store.clone().unwrap();
    speech::remove(&store, speech::find("supertonic-3").unwrap()).unwrap();
    asked(&mut app, 1, true);
    delta(&mut app, 1, "First one. Second");
    assert!(voice.texts().is_empty());
    assert!(app.status.contains("Supertonic 3"), "{}", app.status);
    assert!(!app.voice.speech.is_speaking());
}

/// Try says a sentence in the interface's language with the model asked
/// for; picking a voice says it in that voice.
#[test]
fn try_says_a_sample_in_the_readers_language() {
    use crate::i18n::{Lang, with_lang};
    let (mut app, voice, _) = speech_app("speech-try");
    let task = app.update(Message::Voice(VoiceMsg::TrySpeech("supertonic-3")));
    settle(&mut app, task);
    let said = voice.said.lock().unwrap().last().cloned().unwrap();
    assert_eq!(said.lang, "en");
    assert!(said.text.contains("Clew"), "{}", said.text);
    with_lang(Lang::Fr, || {
        let task = app.update(Message::Voice(VoiceMsg::PickSpeechVoice {
            model: "supertonic-3",
            voice: 7,
        }));
        settle(&mut app, task);
    });
    let said = voice.said.lock().unwrap().last().cloned().unwrap();
    assert_eq!((said.lang.as_str(), said.voice), ("fr", 7));
    assert_ne!(
        said.text,
        "This is how Clew sounds when it reads an answer aloud."
    );
    // In Chinese, Supertonic says it in English; Kokoro in Chinese.
    install(&app, speech::find("kokoro-zh").unwrap());
    with_lang(Lang::Zh, || {
        let task = app.update(Message::Voice(VoiceMsg::TrySpeech("supertonic-3")));
        settle(&mut app, task);
        let said = voice.said.lock().unwrap().last().cloned().unwrap();
        assert_eq!((said.model, said.lang.as_str()), ("supertonic-3", "en"));
        let task = app.update(Message::Voice(VoiceMsg::TrySpeech("kokoro-zh")));
        settle(&mut app, task);
        let said = voice.said.lock().unwrap().last().cloned().unwrap();
        assert_eq!((said.model, said.lang.as_str()), ("kokoro-zh", "zh"));
    });
    // A voice the model does not have is not taken.
    let before = app.voice.prefs.voices.clone();
    let _ = app.update(Message::Voice(VoiceMsg::PickSpeechVoice {
        model: "supertonic-3",
        voice: 99,
    }));
    assert_eq!(app.voice.prefs.voices, before);
}

/// The speech settings are kept in config.toml, and read back; what is
/// not one of the choices is not taken.
#[test]
fn speech_settings_are_kept() {
    let data = test_dir("speech-prefs-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let (mut app, _, _) = speech_app("speech-prefs");
    let _ = app.update(Message::Voice(VoiceMsg::SetSpeak(false)));
    let _ = app.update(Message::Voice(VoiceMsg::SetSpeed(1.25)));
    let task = app.update(Message::Voice(VoiceMsg::PickSpeechVoice {
        model: "supertonic-3",
        voice: 8,
    }));
    settle(&mut app, task);
    let read = VoicePrefs::load();
    assert_eq!(read, app.voice.prefs);
    assert!(!read.speak);
    assert_eq!(read.speed, 1.25);
    assert_eq!(read.voice_for(speech::find("supertonic-3").unwrap()), 8);
    assert_eq!(read.voice_for(speech::find("kokoro-zh").unwrap()), 47);
    let _ = app.update(Message::Voice(VoiceMsg::SetSpeed(9.0)));
    assert_eq!(app.voice.prefs.speed, 1.25);
}

/// Every file a fetch was asked for, and the most it might be.
static FETCHED: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

fn junk_fetch(
    url: &str,
    max_bytes: u64,
    _cancel: &AtomicBool,
    sink: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    FETCHED.lock().unwrap().push((url.to_string(), max_bytes));
    std::thread::sleep(std::time::Duration::from_millis(200));
    sink.write_all(b"not the file").map_err(|e| e.to_string())?;
    progress(12, None);
    Ok(12)
}

/// A speech model is downloaded file by file — only those not here yet,
/// each from its pinned revision and capped at its published size — and
/// each is verified: one that is not as published is refused and leaves
/// nothing. Cancel stops it; Remove deletes it, the voice let go first.
#[test]
fn speech_models_download_verified_file_by_file() {
    let (mut app, voice, _) = speech_app("speech-download");
    let model = speech::find("kokoro-zh").unwrap();
    let store = app.voice.store.clone().unwrap();
    // All but its last file are here.
    install(&app, model);
    let last = model.files.last().unwrap();
    std::fs::remove_file(speech::path(&store, model, last)).unwrap();
    assert!(!app.speech_installed(model));
    app.voice.fetch = junk_fetch;
    FETCHED.lock().unwrap().clear();
    let task = app.update(Message::Voice(VoiceMsg::DownloadSpeech("kokoro-zh")));
    assert!(app.voice.speech.downloads.contains_key("kokoro-zh"));
    // Twice is once.
    let again = app.update(Message::Voice(VoiceMsg::DownloadSpeech("kokoro-zh")));
    settle(&mut app, again);
    settle(&mut app, task);
    assert_eq!(
        FETCHED.lock().unwrap().as_slice(),
        [(model.url(last), last.bytes)]
    );
    assert!(model.url(last).contains(model.revision));
    let error = app
        .voice
        .speech
        .download_errors
        .get("kokoro-zh")
        .cloned()
        .unwrap();
    assert!(error.contains("as published"), "{error}");
    assert!(!speech::path(&store, model, last).exists());
    let dir = speech::dir(&store, model);
    assert!(
        std::fs::read_dir(&dir).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".part")),
        "nothing half-written left"
    );
    // Cancelled: whatever it says after is nobody's.
    let task = app.update(Message::Voice(VoiceMsg::DownloadSpeech("kokoro-zh")));
    let _ = app.update(Message::Voice(VoiceMsg::CancelSpeechDownload("kokoro-zh")));
    assert!(app.voice.speech.downloads.is_empty());
    let before = app.status.clone();
    settle(&mut app, task);
    assert_eq!(app.status, before);
    // Removed: the voice let go of first.
    let task = app.update(Message::Voice(VoiceMsg::RemoveSpeech("kokoro-zh")));
    settle(&mut app, task);
    assert!(!dir.exists());
    assert_eq!(voice.stopped.load(Ordering::Relaxed), 1);
    assert!(app.status.contains("Removed"), "{}", app.status);
}

/// A Stop's close can come after the next question was asked by voice: it
/// stops nothing of the new answer — and an earlier answer that ends late
/// is not read over the new one, from its start.
#[test]
fn a_late_close_of_an_earlier_answer_leaves_the_new_one_be() {
    let (mut app, voice, _) = speech_app("speech-late-close");
    asked(&mut app, 1, true);
    delta(&mut app, 1, "The total adds every price. Then");
    // Stop: the turn stays open until the server says it is closed.
    let _ = app.update(Message::Voice(VoiceMsg::StopSpeaking));
    asked(&mut app, 2, true);
    delta(&mut app, 2, "The discount is checked first. Then");
    assert!(app.voice.speech.is_speaking());
    let before = voice.texts().len();
    // The first answer's close lands now.
    ended(&mut app, 1, StreamOutcome::Stopped);
    assert!(app.voice.speech.is_speaking(), "the new answer goes on");
    delta(&mut app, 2, " it is applied. And");
    assert_eq!(voice.texts().len(), before + 1, "{:?}", voice.texts());
    // An earlier answer that ends late, Done, is not read at all.
    let said = voice.texts().len();
    app.proj.ask_turns[0].streaming = true;
    ended(&mut app, 1, StreamOutcome::Done);
    assert_eq!(voice.texts().len(), said, "{:?}", voice.texts());
}

/// With Keep listening on, a voice that fails as the answer ends still
/// leaves the microphone to open: the reader did not stop it.
#[test]
fn keep_listening_survives_a_voice_that_fails() {
    let (mut app, voice, _) = speech_app("speech-keep-listening-fails");
    let model = app.voice.prefs.model();
    let store = app.voice.store.clone().unwrap();
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.path(model), b"a model").unwrap();
    app.voice.open_mic = |_| Ok(Box::new(Quiet) as Box<dyn crate::app::voice::Listening>);
    app.ask_view = AskView::Compact;
    let _ = app.update(Message::Voice(VoiceMsg::SetKeepListening(true)));
    asked(&mut app, 1, true);
    // The answer ends while its sentence is still being made, and that
    // sentence fails: clew-speak is gone.
    let _ = app.on_ask_delta(1, "The total adds every price.".into());
    *voice.fail.lock().unwrap() = Some(SayError::Voice("the voice ended unexpectedly".into()));
    let task = app.on_ask_stream_ended(1, StreamOutcome::Done);
    settle(&mut app, task);
    assert!(!app.voice.speech.is_speaking());
    assert!(
        matches!(app.voice.phase, VoicePhase::Listening { .. }),
        "{:?} — {}",
        app.voice.phase,
        app.status
    );
}

/// The time an output has nothing to play is no stall: a sentence made
/// after a long wait, its device still reporting an old hiccup, plays on.
#[test]
fn a_wait_between_sentences_is_no_stall() {
    let (mut app, _voice, output) = speech_app("speech-gap");
    asked(&mut app, 1, true);
    delta(&mut app, 1, "First one. Then");
    let tick = |app: &mut App| {
        let task = app.update(Message::Voice(VoiceMsg::SpeakTick));
        settle(app, task);
    };
    // All of it played; the device reported a hiccup it played through.
    {
        let mut o = output.lock().unwrap();
        o.progress += 200;
        o.idle = true;
        o.error = Some("the speaker failed: an underrun".into());
    }
    tick(&mut app);
    // A long wait for the next sentence (longer than either limit).
    std::thread::sleep(std::time::Duration::from_millis(400));
    tick(&mut app);
    let opened = output.lock().unwrap().opened;
    // The next sentence comes, and a tick lands before it starts playing.
    delta(&mut app, 1, " the second one. And");
    tick(&mut app);
    assert!(app.voice.speech.is_speaking(), "{}", app.status);
    assert_eq!(output.lock().unwrap().opened, opened, "not opened again");
}

/// An output that stops playing is found out while sentences still come
/// to it: each one given to it is no time it played.
#[test]
fn a_stopped_output_is_found_out_while_sentences_come() {
    let (mut app, _voice, output) = speech_app("speech-stuck-output");
    asked(&mut app, 1, true);
    for i in 0..12 {
        delta(&mut app, 1, &format!("Sentence number {i} is here. "));
        std::thread::sleep(std::time::Duration::from_millis(80));
        let task = app.update(Message::Voice(VoiceMsg::SpeakTick));
        settle(&mut app, task);
    }
    assert!(
        output.lock().unwrap().opened >= 2,
        "the stopped output was never found out"
    );
}

/// Speech ends with the conversation it belongs to: a new one, another
/// session, another project — also once its answer is written.
#[test]
fn speech_ends_with_its_conversation() {
    let (mut app, voice, _) = speech_app("speech-new-session");
    asked(&mut app, 1, true);
    delta(&mut app, 1, "The total adds every price. Then it rounds.");
    ended(&mut app, 1, StreamOutcome::Done);
    assert!(app.voice.speech.is_speaking());
    let task = app.update(Message::Ask(AskMsg::NewSession));
    settle(&mut app, task);
    assert!(!app.voice.speech.is_speaking());
    let _ = voice;
}

/// Deleting the session on screen ends what is said of it — another
/// session's deletion does not.
#[test]
fn speech_ends_with_the_session_deleted() {
    let (mut app, _voice, _) = speech_app("speech-delete-session");
    app.proj.ask_session = Some(7);
    asked(&mut app, 1, true);
    delta(&mut app, 1, "The total adds every price. Then it rounds.");
    ended(&mut app, 1, StreamOutcome::Done);
    assert!(app.voice.speech.is_speaking());
    let task = app.update(Message::Ask(AskMsg::DeleteSession(8)));
    settle(&mut app, task);
    assert!(app.voice.speech.is_speaking(), "another session went");
    let task = app.update(Message::Ask(AskMsg::DeleteSession(7)));
    settle(&mut app, task);
    assert!(!app.voice.speech.is_speaking());
}

/// Each sentence comes to the output at the output's own rate: made at it
/// off the UI thread, not on it.
#[test]
fn sentences_come_at_the_outputs_rate() {
    let (mut app, voice, output) = speech_app("speech-rate");
    *voice.rate.lock().unwrap() = 2000;
    asked(&mut app, 1, true);
    delta(&mut app, 1, "First one. Second one. Third");
    let script = output.lock().unwrap();
    assert!(!script.rates.is_empty());
    assert!(
        script.rates.iter().all(|r| *r == 1000),
        "{:?}",
        script.rates
    );
    // Two words, made as 400 samples at 2 kHz: 200 at the output's 1 kHz.
    assert_eq!(script.played[0], 200, "{:?}", script.played);
}

/// A project to ask in by retrieval: a selection pinned as its grounding (no
/// index needed), and a chat model that is never reached.
fn retrieval_app(tag: &str) -> App {
    let (mut app, _voice, _) = speech_app(tag);
    let root = app.proj.project.as_ref().unwrap().root.clone();
    app.proj.ask_pins.push(crate::app::model::AskPin {
        rel: "README.md".into(),
        file: root.join("README.md"),
        line: 1,
        code: "# shop".into(),
    });
    app.llm_config_cache = Some(Some(super::unreachable_llm()));
    app
}

/// An agent turn on `stream`, for `question` asked aloud or typed, that
/// fails at once: what falls back to the index, as a task.
fn agent_fails(app: &mut App, spoken: bool) -> Task<Message> {
    let stream = app.mint_request_id();
    asked(app, stream, spoken);
    app.proj.link.agent_stream = Some(stream);
    app.update(Message::Ask(AskMsg::AgentTurnEnded {
        stamp: app.stamp(),
        stream,
        outcome: StreamOutcome::Failed("agent unavailable".into()),
    }))
}

/// Whether the newest turn's question was said aloud — its answer read.
fn newest_spoken(app: &App) -> bool {
    app.proj.ask_turns.last().expect("a turn").spoken
}

/// An agent turn that fails at once falls back to the index — and the
/// answer to a question said aloud is said aloud there too, whatever is
/// typed into the input while the index is read.
#[test]
fn the_fallback_answer_to_a_spoken_question_is_spoken() {
    let mut app = retrieval_app("speech-fallback");
    let task = agent_fails(&mut app, true);
    assert!(app.proj.asking, "the index took the question over");
    let _ = app.update(Message::Ask(AskMsg::InputChanged("and the tax".into())));
    settle(&mut app, task);
    assert_eq!(app.proj.ask_turns.len(), 1, "the fallback's own turn");
    assert!(newest_spoken(&app), "its answer is read aloud");
    assert!(!app.proj.ask_spoken, "what was typed is not taken for said");
}

/// The next question, said into the input while an agent turn runs, is
/// still one said when that turn falls back — and the fallback's answer to
/// a typed question is not read.
#[test]
fn a_question_said_meanwhile_keeps_its_voice() {
    let mut app = retrieval_app("speech-said-meanwhile");
    app.proj.ask_input = "and the tax?".into();
    app.proj.ask_spoken = true;
    let task = agent_fails(&mut app, false);
    settle(&mut app, task);
    assert!(
        !newest_spoken(&app),
        "a typed question's answer is not read"
    );
    assert_eq!(app.proj.ask_input, "and the tax?");
    assert!(app.proj.ask_spoken, "the question said meanwhile still is");
}

/// A question said aloud takes its voice with it when asked: stopped while
/// the index is read, the next one typed is not taken for one said. One
/// that fails comes back into the input as said.
#[test]
fn a_spoken_question_takes_its_voice_with_it() {
    let mut app = retrieval_app("speech-flag-taken");
    app.proj.ask_input = "what does total do?".into();
    app.proj.ask_spoken = true;
    let retrieving = app.update(Message::Ask(AskMsg::Submit));
    assert!(app.proj.asking);
    assert!(!app.proj.ask_spoken, "the question took it");
    assert!(
        run_task(retrieving)
            .iter()
            .any(|m| matches!(m, Message::Ask(AskMsg::Retrieved { spoken: true, .. }))),
        "to its retrieval"
    );
    let _ = app.update(Message::Ask(AskMsg::Stop));
    assert!(!app.proj.ask_spoken, "nor did the Stop leave it behind");
    // Its retrieval fails: it is back, still the question said.
    app.proj.ask_input = "what does total do?".into();
    app.proj.ask_spoken = true;
    let _ = app.update(Message::Ask(AskMsg::Submit));
    let stream = app.proj.inflight.ask_retrieval.as_ref().unwrap().0;
    let _ = app.on_ask_retrieved(
        stream,
        "what does total do?".into(),
        true,
        None,
        Err("timed out".into()),
    );
    assert_eq!(app.proj.ask_input, "what does total do?");
    assert!(app.proj.ask_spoken);
}

/// With MCP servers to reach first, a question said aloud is still one
/// said once they answer, whatever is typed meanwhile.
#[test]
fn a_spoken_question_waits_for_mcp_servers_with_its_voice() {
    let data = test_dir("speech-mcp-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    // A server to reach first, which is not there: it answers at once.
    std::fs::write(
        data.join("config.toml"),
        "[mcp.servers.gone]\ncommand = \"/nonexistent/clew-mcp-server\"\n",
    )
    .unwrap();
    let mut app = retrieval_app("speech-mcp-gather");
    let _rx = super::attach_server(&mut app);
    app.proj.ask_input = "what does total do?".into();
    app.proj.ask_spoken = true;
    let gathering = app.update(Message::Ask(AskMsg::Submit));
    assert!(
        app.proj.inflight.ask_gather.is_some(),
        "the servers are reached first"
    );
    let _ = app.update(Message::Ask(AskMsg::InputChanged("typed".into())));
    // The servers answered (the one there is not): the turn starts — its
    // pump left alone, as no server answers it here.
    let gathered = run_task(gathering)
        .into_iter()
        .find(|m| matches!(m, Message::Ask(AskMsg::McpGathered { .. })))
        .expect("the servers answered");
    let _pump = app.update(gathered);
    assert!(newest_spoken(&app));
    assert!(!app.proj.ask_spoken, "what was typed is not taken for said");
}

/// While a voice model's files are removed — or downloaded — in any
/// window, nothing is said with it: what would be is passed over.
#[test]
fn nothing_is_said_with_a_model_being_removed() {
    let (mut app, voice, _) = speech_app("speech-removing");
    let store = app.voice.store.clone().unwrap();
    let model = speech::find("supertonic-3").unwrap();
    let claim = Claim::take(speech::dir(&store, model), ClaimedFor::Download).unwrap();
    asked(&mut app, 1, true);
    delta(&mut app, 1, "First one. Second one. Third");
    assert!(voice.texts().is_empty());
    assert!(!app.status.contains("download"), "{}", app.status);
    drop(claim);
    // Removing a model being downloaded is refused, saying so.
    let claim = Claim::take(speech::dir(&store, model), ClaimedFor::Download).unwrap();
    let task = app.update(Message::Voice(VoiceMsg::RemoveSpeech("supertonic-3")));
    settle(&mut app, task);
    assert!(app.speech_installed(model));
    assert!(app.status.contains("already downloading"), "{}", app.status);
    drop(claim);
}

/// A model being removed says so to a second Remove, here or in another
/// window, and to a download — not that it is downloading.
#[test]
fn a_model_being_removed_says_so() {
    let (mut app, _voice, _) = speech_app("speech-removed-twice");
    let removing = app.update(Message::Voice(VoiceMsg::RemoveSpeech("supertonic-3")));
    let _ = app.update(Message::Voice(VoiceMsg::RemoveSpeech("supertonic-3")));
    assert!(app.status.contains("being removed"), "{}", app.status);
    app.status.clear();
    let _ = app.update(Message::Voice(VoiceMsg::DownloadSpeech("supertonic-3")));
    assert!(app.status.contains("being removed"), "{}", app.status);
    settle(&mut app, removing);
    assert!(app.status.starts_with("Removed"), "{}", app.status);
}

/// Removing a model ends what is being said with it — not what another
/// model says.
#[test]
fn removing_a_model_ends_only_what_it_says() {
    let (mut app, _voice, _) = speech_app("speech-remove-other");
    install(&app, speech::find("kokoro-zh").unwrap());
    asked(&mut app, 1, true);
    delta(&mut app, 1, "The total adds every price. Then it rounds.");
    ended(&mut app, 1, StreamOutcome::Done);
    assert!(app.voice.speech.is_speaking());
    let task = app.update(Message::Voice(VoiceMsg::RemoveSpeech("kokoro-zh")));
    settle(&mut app, task);
    assert!(!app.speech_installed(speech::find("kokoro-zh").unwrap()));
    assert!(
        app.voice.speech.is_speaking(),
        "another model's speech goes on"
    );
    let task = app.update(Message::Voice(VoiceMsg::RemoveSpeech("supertonic-3")));
    settle(&mut app, task);
    assert!(!app.voice.speech.is_speaking());
}

/// Each window saves the voice setting it changed, and only that: a
/// window whose settings are older than another's keeps the other's.
#[test]
fn windows_keep_each_others_voice_settings() {
    let data = test_dir("speech-two-windows-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let (mut first, _, _) = speech_app("speech-window-a");
    let (mut second, _, _) = speech_app("speech-window-b");
    let _ = first.update(Message::Voice(VoiceMsg::SetSpeed(1.25)));
    let task = first.update(Message::Voice(VoiceMsg::PickSpeechVoice {
        model: "supertonic-3",
        voice: 6,
    }));
    settle(&mut first, task);
    // The second window still reads 1× and the default voice.
    let _ = second.update(Message::Voice(VoiceMsg::SetSpeak(false)));
    let _ = second.update(Message::Voice(VoiceMsg::SetRunCommands(false)));
    let read = VoicePrefs::load();
    assert_eq!(read.speed, 1.25);
    assert_eq!(read.voice_for(speech::find("supertonic-3").unwrap()), 6);
    assert!(!read.speak);
    assert!(!read.run_commands);
}
