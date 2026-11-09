//! Speech (`app::speech`): answers to spoken questions read aloud — by a
//! test's voice, which says what it is asked to as a few samples, onto a
//! test's output, which keeps what it is given — and stopped when the
//! reader asks; Keep listening waiting for Clew to finish; the speech
//! settings and models.

use super::*;
use crate::app::speech::{Playing, Synthesizer};
use crate::app::voice::{Claim, ClaimedFor, ReadAloud, VoicePhase, VoicePrefs};
use clew_protocol::StreamOutcome;
use clew_voice::speaker::SayError;
use clew_voice::speech::{self, VoiceModel};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

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
    /// The models it was asked to let go of, in turn.
    stopped: Mutex<Vec<&'static str>>,
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
    fn stop_while(&self, model: &'static VoiceModel, work: &mut dyn FnMut()) {
        self.stopped.lock().unwrap().push(model.id);
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
    /// Held: the reader talks over Clew.
    paused: bool,
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
    fn pause(&self) {
        self.0.lock().unwrap().paused = true;
    }
    fn resume(&self) {
        self.0.lock().unwrap().paused = false;
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
    assert!(app.voice.takes_esc(), "Esc has it to stop");
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
    assert!(!app.voice.takes_esc());
    // Typed: not said.
    asked(&mut app, 2, false);
    delta(&mut app, 2, "The total adds them. Then");
    ended(&mut app, 2, StreamOutcome::Done);
    assert_eq!(voice.texts().len(), 2);
    // Turned off: not said.
    let _ = app.update(Message::Voice(VoiceMsg::SetSpeak(ReadAloud::Never)));
    asked(&mut app, 3, true);
    delta(&mut app, 3, "The total adds them. Then");
    ended(&mut app, 3, StreamOutcome::Done);
    assert_eq!(voice.texts().len(), 2);
    // Every answer: a typed question's too.
    let _ = app.update(Message::Voice(VoiceMsg::SetSpeak(ReadAloud::Always)));
    asked(&mut app, 4, false);
    delta(&mut app, 4, "The total adds them. Then");
    assert_eq!(
        voice.texts().get(2).map(String::as_str),
        Some("The total adds them.")
    );
    // Turned back to spoken questions only, what was being read of a typed
    // one stops.
    let _ = app.update(Message::Voice(VoiceMsg::SetSpeak(ReadAloud::Spoken)));
    assert!(!app.voice.speech.is_speaking());
    let said = voice.texts().len();
    ended(&mut app, 4, StreamOutcome::Done);
    assert_eq!(voice.texts().len(), said);
}

/// An agent step's text is said as it is written, not once it is whole: a
/// remark before tool calls is said through and taken out of the answer —
/// the tool calls show what it went on to do — and the answer the next step
/// writes is said from its first sentence, none of the remark again.
#[test]
fn a_steps_text_is_said_as_it_is_written() {
    let (mut app, voice, _output) = speech_app("speech-drafts");
    asked(&mut app, 1, true);
    let draft = |app: &mut App, text: &str| {
        let task = app.on_ask_draft(1, text.into());
        settle(app, task);
    };
    let draft_end = |app: &mut App, kept: bool| {
        let task = app.on_ask_draft_end(1, kept);
        settle(app, task);
    };
    // A remark, then its tool calls.
    draft(&mut app, "Let me look at the pricing code. Then");
    assert_eq!(
        voice.texts(),
        ["Let me look at the pricing code."],
        "said as it came"
    );
    assert_eq!(
        app.proj.ask_turns[0].answer_md, "Let me look at the pricing code. Then",
        "shown as it came"
    );
    draft_end(&mut app, false);
    assert_eq!(
        app.proj.ask_turns[0].answer_md, "",
        "the remark is not the answer"
    );
    assert!(app.proj.ask_turns[0].answer.is_empty());
    assert_eq!(voice.texts(), ["Let me look at the pricing code.", "Then"]);
    // The answer, written in the next step: said from its first sentence.
    draft(&mut app, "The total adds tax last. It rounds to cents. And");
    assert_eq!(
        voice.texts()[2..],
        ["The total adds tax last.", "It rounds to cents."]
    );
    draft_end(&mut app, true);
    assert_eq!(
        app.proj.ask_turns[0].answer_md,
        "The total adds tax last. It rounds to cents. And"
    );
    ended(&mut app, 1, StreamOutcome::Done);
    assert_eq!(voice.texts().last().map(String::as_str), Some("And"));
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
    app.voice.open_mic = |_, _| Err("no microphone is available".into());
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

/// Not to be talked over, the microphone, on, opens once Clew has said
/// all of the answer — not while it speaks, which it would hear; the
/// reader stopping Clew opens it at once, as does an answer with nothing to
/// say.
#[test]
fn the_microphone_waits_for_clew_to_finish() {
    let (mut app, _voice, output) = speech_app("speech-mic-waits");
    // Not talked over: talked over, it listens through (below).
    app.voice.prefs.talk_over = false;
    let model = app.voice.prefs.model();
    let store = app.voice.store.clone().unwrap();
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.path(model), b"a model").unwrap();
    app.voice.open_mic = |_, _| Ok(Box::new(Quiet) as Box<dyn crate::app::voice::Listening>);
    app.ask_view = AskView::Compact;
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(app.voice.is_listening(), "{}", app.status);
    asked(&mut app, 1, true);
    delta(&mut app, 1, "The total adds every price.");
    assert_eq!(app.voice.phase, VoicePhase::Idle, "not while Clew speaks");
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
    // Stopped by the reader: it listens at once.
    asked(&mut app, 2, true);
    delta(&mut app, 2, "The total adds every price. Then");
    assert_eq!(app.voice.phase, VoicePhase::Idle);
    let _ = app.update(Message::Voice(VoiceMsg::StopSpeaking));
    assert!(app.voice.is_listening(), "{:?}", app.voice.phase);
    ended(&mut app, 2, StreamOutcome::Done);
    assert!(app.voice.is_listening());
    // Nothing to say (all code): it listens at once.
    asked(&mut app, 3, true);
    delta(&mut app, 3, "```\nx = 1\n```");
    ended(&mut app, 3, StreamOutcome::Done);
    assert!(matches!(app.voice.phase, VoicePhase::Listening { .. }));
    // Off, it stays off.
    app.voice.forget_last_press();
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    asked(&mut app, 4, true);
    delta(&mut app, 4, "The total adds every price.");
    ended(&mut app, 4, StreamOutcome::Done);
    all_played(&mut app, &output);
    assert_eq!(app.voice.phase, VoicePhase::Idle);
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
    fn speech_over(&self) -> Option<f32> {
        None
    }
    fn recent(&self, _seconds: f32) -> (Vec<f32>, u32) {
        (Vec::new(), 16_000)
    }
    fn full(&self) -> bool {
        false
    }
    fn error(&self) -> Option<String> {
        None
    }
    fn take(&mut self) -> clew_voice::capture::Recording {
        clew_voice::capture::Recording {
            samples: Vec::new(),
            rate: 16_000,
        }
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
    delta(&mut app, 1, "This is. That is. Third");
    ended(&mut app, 1, StreamOutcome::Done);
    assert_eq!(voice.texts(), ["That is.", "Third"], "the next was said");
    all_played(&mut app, &output);
    *voice.fail.lock().unwrap() = Some(SayError::Voice(
        "clew-speak, which reads answers aloud, is not installed next to Clew".into(),
    ));
    asked(&mut app, 2, true);
    delta(&mut app, 2, "This is. That is. Third");
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
    delta(&mut app, 3, "This is. Second");
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
    delta(&mut app, 4, "This is. Second");
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
    delta(&mut app, 5, "This is. Second");
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
    delta(&mut app, 1, "This is. Second");
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
    let _ = app.update(Message::Voice(VoiceMsg::SetSpeak(ReadAloud::Never)));
    let _ = app.update(Message::Voice(VoiceMsg::SetSpeed(1.25)));
    let task = app.update(Message::Voice(VoiceMsg::PickSpeechVoice {
        model: "supertonic-3",
        voice: 8,
    }));
    settle(&mut app, task);
    let read = VoicePrefs::load();
    assert_eq!(read, app.voice.prefs);
    assert_eq!(read.read_aloud, ReadAloud::Never);
    assert_eq!(read.speed, 1.25);
    assert_eq!(read.voice_for(speech::find("supertonic-3").unwrap()), 8);
    assert_eq!(read.voice_for(speech::find("kokoro-zh").unwrap()), 47);
    let _ = app.update(Message::Voice(VoiceMsg::SetSpeed(9.0)));
    assert_eq!(app.voice.prefs.speed, 1.25);
    // Kept as a switch by an earlier Clew: on read the answers to spoken
    // questions, off none; what is none of these is not taken.
    for (kept, read_aloud) in [
        (toml::Value::Boolean(true), ReadAloud::Spoken),
        (toml::Value::Boolean(false), ReadAloud::Never),
        (toml::Value::String("always".into()), ReadAloud::Always),
        (toml::Value::String("loudly".into()), ReadAloud::Spoken),
    ] {
        clew_core::globalconfig::edit(|root| {
            if let Some(voice) = root.get_mut("voice").and_then(|v| v.as_table_mut()) {
                voice.insert("speak".into(), kept.clone());
            }
        })
        .unwrap();
        assert_eq!(VoicePrefs::load().read_aloud, read_aloud, "{kept}");
    }
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
    assert!(error.contains("does not match the published"), "{error}");
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
    assert_eq!(*voice.stopped.lock().unwrap(), ["kokoro-zh"], "its own");
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

/// With the microphone on, a voice that fails as the answer ends still
/// leaves the microphone to open: the reader did not turn it off.
#[test]
fn the_microphone_survives_a_voice_that_fails() {
    let (mut app, voice, _) = speech_app("speech-keep-listening-fails");
    let model = app.voice.prefs.model();
    let store = app.voice.store.clone().unwrap();
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.path(model), b"a model").unwrap();
    app.voice.open_mic = |_, _| Ok(Box::new(Quiet) as Box<dyn crate::app::voice::Listening>);
    app.ask_view = AskView::Compact;
    // On already, as when the question was said.
    app.voice.mic_on = true;
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
    delta(&mut app, 1, "This is. Then");
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
    delta(&mut app, 1, " this is the second. And");
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

/// A question queued while a spoken answer came in is asked once Clew has
/// said that answer, not as soon as its last words have come: asked then,
/// as a question asked anew it stopped Clew mid-answer, and the rest of
/// what the reader was listening to went unsaid. Its Send now still asks
/// it at once, over what is being said.
#[test]
fn a_queued_question_waits_for_the_answer_to_be_said() {
    let (mut app, voice, output) = speech_app("speech-queue-waits");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    app.proj.ask_pins.push(crate::app::model::AskPin {
        rel: "README.md".into(),
        file: root.join("README.md"),
        line: 1,
        code: "# shop".into(),
    });
    app.llm_config_cache = Some(Some(super::unreachable_llm()));
    let answered = |app: &mut App, question: &str| {
        let stream = app.mint_request_id();
        asked(app, stream, true);
        app.proj.link.agent_stream = Some(stream);
        delta(app, stream, "The total adds every price. Then");
        app.proj.ask_input = question.into();
        let _ = app.update(Message::Ask(AskMsg::Submit));
        delta(app, stream, " it rounds.");
        let task = app.update(Message::Ask(AskMsg::AgentTurnEnded {
            stamp: app.stamp(),
            stream,
            outcome: StreamOutcome::Done,
        }));
        settle(app, task);
    };
    answered(&mut app, "and the tax?");
    assert!(app.voice.speech.is_speaking(), "stopped mid-answer");
    assert_eq!(voice.texts().len(), 2, "{:?}", voice.texts());
    assert_eq!(
        app.proj.ask_queue,
        vec![("and the tax?".to_string(), false)],
        "asked over the answer being said"
    );
    // Said: the next is asked. (Its answer, from a model never reached, is
    // not waited for.)
    output.lock().unwrap().idle = true;
    let _ = app.update(Message::Voice(VoiceMsg::SpeakTick));
    std::thread::sleep(std::time::Duration::from_millis(400));
    let _ = app.update(Message::Voice(VoiceMsg::SpeakTick));
    assert!(!app.voice.speech.is_speaking());
    assert!(app.proj.ask_queue.is_empty(), "not asked once said");
    assert!(app.proj.inflight.ask_retrieval.is_some(), "{}", app.status);
    let _ = app.update(Message::Ask(AskMsg::Stop));

    // Send now: asked at once, what is being said stopped for it.
    answered(&mut app, "and the discount?");
    assert!(app.voice.speech.is_speaking());
    let _ = app.update(Message::Ask(AskMsg::SendQueuedNow(0)));
    assert!(!app.voice.speech.is_speaking());
    assert!(app.proj.ask_queue.is_empty(), "Send now waited");
    assert!(app.proj.inflight.ask_retrieval.is_some(), "{}", app.status);
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
    delta(&mut app, 1, "This is. That is. Third");
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
    delta(&mut app, 1, "This is. That is. Third");
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
    let _ = second.update(Message::Voice(VoiceMsg::SetSpeak(ReadAloud::Always)));
    let _ = second.update(Message::Voice(VoiceMsg::SetRunCommands(false)));
    let read = VoicePrefs::load();
    assert_eq!(read.speed, 1.25);
    assert_eq!(read.voice_for(speech::find("supertonic-3").unwrap()), 6);
    assert_eq!(read.read_aloud, ReadAloud::Always);
    assert!(!read.run_commands);
}

thread_local! {
    /// How long the talking microphone has been quiet since words were
    /// said over Clew; `None` while none were.
    static TALK: std::cell::Cell<Option<f32>> = const { std::cell::Cell::new(None) };
    /// Where an unbroken run of speech said over Clew began, in seconds;
    /// `None` while there was none — whatever [`TALK`] says.
    static OVER: std::cell::Cell<Option<f32>> = const { std::cell::Cell::new(None) };
    /// Whether the microphone was opened to take Clew's own voice out.
    static CANCELLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Whether the microphone has gone away.
    static GONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// How many times a microphone was opened.
    static OPENED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many seconds of what was heard the recognizer was given, by the
/// words it hears: it works off the test's thread.
static GIVEN: std::sync::LazyLock<Mutex<std::collections::HashMap<String, f32>>> =
    std::sync::LazyLock::new(Default::default);

fn given(heard: &str) -> Option<f32> {
    GIVEN.lock().unwrap().get(heard).copied()
}

/// A microphone the test talks into ([`TALK`]).
struct Talker;

impl crate::app::voice::Listening for Talker {
    fn level(&self) -> f32 {
        0.2
    }
    fn silence_after_speech(&self) -> Option<f32> {
        TALK.with(|t| t.get())
    }
    fn speech_over(&self) -> Option<f32> {
        OVER.with(|o| o.get())
    }
    fn recent(&self, _seconds: f32) -> (Vec<f32>, u32) {
        (Vec::new(), 16_000)
    }
    fn full(&self) -> bool {
        false
    }
    fn error(&self) -> Option<String> {
        GONE.with(|g| g.get())
            .then(|| "the microphone went away".to_string())
    }
    fn take(&mut self) -> clew_voice::capture::Recording {
        heard()
    }
    fn stop(self: Box<Self>) -> clew_voice::capture::Recording {
        heard()
    }
}

/// What [`Talker`] heard.
fn heard() -> clew_voice::capture::Recording {
    // A blip of what was left of Clew's voice at a quarter of a second;
    // then quiet; a second said from the second; half a second of quiet.
    let said: Vec<f32> = (0..56_000)
        .map(|i| {
            if (4_000..8_000).contains(&i) || (32_000..48_000).contains(&i) {
                0.3 * (i as f32 * 0.07).sin()
            } else {
                0.0
            }
        })
        .collect();
    clew_voice::capture::Recording {
        samples: said,
        rate: 16_000,
    }
}

fn talking_mic(
    _max_seconds: u32,
    cancel_echo: bool,
) -> Result<Box<dyn crate::app::voice::Listening>, String> {
    CANCELLED.with(|c| c.set(cancel_echo));
    OPENED.with(|o| o.set(o.get() + 1));
    Ok(Box::new(Talker))
}

/// A recognizer that hears `0`.
struct Hears(String);

impl crate::app::voice::Transcriber for Hears {
    fn transcribe(
        &self,
        _model: &Path,
        audio: &[f32],
        _options: &clew_voice::whisper::Options,
        _cancel: &AtomicBool,
    ) -> Result<String, String> {
        GIVEN
            .lock()
            .unwrap()
            .insert(self.0.clone(), audio.len() as f32 / 16_000.0);
        Ok(self.0.clone())
    }
}

/// Clew reading an answer aloud with the microphone on, to be talked over;
/// what the reader is heard to say is `heard`.
fn talked_over(tag: &str, heard: &str) -> (App, Arc<Mutex<OutputScript>>) {
    let (mut app, _voice, output) = speech_app(tag);
    let model = app.voice.prefs.model();
    let store = app.voice.store.clone().unwrap();
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.path(model), b"a model").unwrap();
    app.voice.open_mic = talking_mic;
    app.voice.transcriber = Arc::new(Hears(heard.into()));
    app.ask_view = AskView::Compact;
    TALK.with(|t| t.set(None));
    OVER.with(|o| o.set(None));
    GONE.with(|g| g.set(false));
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(
        CANCELLED.with(|c| c.get()),
        "opened to take Clew's voice out"
    );
    asked(&mut app, 1, true);
    delta(
        &mut app,
        1,
        "The total adds every price. Then the discount is taken off.",
    );
    // Clew speaks, and the microphone listens through it.
    assert!(app.voice.speech.is_speaking());
    assert!(
        matches!(app.voice.phase, VoicePhase::Listening { .. }),
        "listening while Clew speaks: {:?}",
        app.voice.phase
    );
    ended(&mut app, 1, StreamOutcome::Done);
    assert!(app.voice.speech.is_speaking());
    (app, output)
}

/// The reader talks over Clew — an unbroken run of speech from the second
/// second, as [`Talker`] records it — and has been quiet since for
/// `quiet` seconds.
fn talk(quiet: Option<f32>) {
    TALK.with(|t| t.set(quiet));
    OVER.with(|o| o.set(Some(2.0)));
}

fn voice_tick(app: &mut App) {
    let task = app.update(Message::Voice(VoiceMsg::Tick));
    settle(app, task);
}

/// Talked over, Clew is held at the first words said over it — the
/// microphone listening all along — and stops once they are heard to be
/// the reader's.
#[test]
fn talking_over_clew_holds_it_then_stops_it() {
    let (mut app, output) = talked_over("speech-talk-over", "wait");
    assert!(
        matches!(app.voice.phase, VoicePhase::Listening { .. }),
        "listening while Clew speaks: {:?}",
        app.voice.phase
    );
    // Words said over Clew: it is held.
    talk(Some(0.1));
    voice_tick(&mut app);
    assert!(app.voice.speech.held());
    assert!(output.lock().unwrap().paused, "faded out");
    assert!(app.voice.speech.is_speaking(), "held, not stopped");
    // Held, the output plays nothing: no stall — it is not opened again.
    let opened = output.lock().unwrap().opened;
    for _ in 0..3 {
        std::thread::sleep(std::time::Duration::from_millis(120));
        let task = app.update(Message::Voice(VoiceMsg::SpeakTick));
        settle(&mut app, task);
    }
    assert!(
        app.voice.speech.is_speaking(),
        "not taken for a stalled speaker"
    );
    assert_eq!(output.lock().unwrap().opened, opened, "not opened again");
    assert!(output.lock().unwrap().paused);
    // What was said ends, and is heard: Clew stops.
    talk(Some(crate::app::voice::END_SILENCE + 0.1));
    voice_tick(&mut app);
    assert!(!app.voice.speech.is_speaking(), "{}", app.status);
    assert!(!app.voice.speech.held());
}

/// What is left of Clew's own voice once its echo is taken out — bursts
/// the looser test takes for speech, but no unbroken run — neither holds
/// Clew nor ends anything, however long it lasts: nothing is worked out of
/// it, and Clew speaks on.
#[test]
fn what_is_left_of_clews_voice_holds_nothing() {
    let (mut app, output) = talked_over("speech-talk-left", "the total price");
    TALK.with(|t| t.set(Some(crate::app::voice::END_SILENCE + 0.5)));
    OVER.with(|o| o.set(None));
    for _ in 0..3 {
        voice_tick(&mut app);
    }
    assert!(!app.voice.speech.held(), "not held");
    assert!(!output.lock().unwrap().paused);
    assert!(
        matches!(app.voice.phase, VoicePhase::Listening { .. }),
        "still listening: {:?}",
        app.voice.phase
    );
    assert_eq!(given("the total price"), None, "nothing worked out");
    assert!(app.voice.speech.is_speaking());
    assert_eq!(app.proj.ask_turns.len(), 1, "nothing asked");
}

/// What is worked out of words said over Clew starts where they began —
/// half a second before — not with what was heard of Clew before them.
#[test]
fn what_was_said_over_clew_is_worked_out_from_where_it_began() {
    let (mut app, _output) = talked_over("speech-talk-from", "wait for me");
    talk(Some(0.1));
    voice_tick(&mut app);
    assert!(app.voice.speech.held());
    talk(Some(crate::app::voice::END_SILENCE + 0.1));
    voice_tick(&mut app);
    // The second said from the second, with the padding speech is given:
    // 300 ms before, 450 after — not the blip at a quarter of a second.
    let given = given("wait for me").expect("worked out");
    assert!((given - 1.75).abs() < 0.05, "{given} s");
    assert!(!app.voice.speech.is_speaking(), "the reader's: Clew stops");
}

/// Where words said over Clew began is of what that microphone heard: one
/// opened afresh — the last gone away while Clew was held — keeps all it
/// hears.
#[test]
fn where_words_over_clew_began_goes_with_the_microphone() {
    let (mut app, _output) = talked_over("speech-talk-gone", "start again please");
    talk(Some(0.1));
    voice_tick(&mut app);
    assert!(app.voice.speech.held());
    GONE.with(|g| g.set(true));
    voice_tick(&mut app);
    assert!(!app.voice.mic_on, "{}", app.status);
    GONE.with(|g| g.set(false));
    // Pressed again — not the key repeating.
    std::thread::sleep(crate::app::voice::TOGGLE_DEBOUNCE + std::time::Duration::from_millis(50));
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(
        matches!(app.voice.phase, VoicePhase::Listening { .. }),
        "{:?} {}",
        app.voice.phase,
        app.status
    );
    // Said into the new microphone, Clew still held: all of it is heard,
    // from the blip at a quarter of a second.
    TALK.with(|t| t.set(Some(crate::app::voice::END_SILENCE + 0.1)));
    voice_tick(&mut app);
    let given = given("start again please").expect("worked out");
    assert!((given - 3.45).abs() < 0.05, "{given} s");
}

/// The microphone that hears Clew is kept open while it may be: after words
/// said over Clew, and after a while with nothing said — opened anew, its
/// echo canceller would learn the room again, and let Clew's voice through
/// meanwhile.
#[test]
fn the_microphone_hearing_clew_is_kept_open() {
    let (mut app, _output) = talked_over("speech-talk-kept", "the total adds every price");
    let opened = OPENED.with(|o| o.get());
    talk(Some(0.1));
    voice_tick(&mut app);
    assert!(app.voice.speech.held());
    talk(Some(crate::app::voice::END_SILENCE + 0.1));
    voice_tick(&mut app);
    assert!(app.voice.speech.is_speaking() && !app.voice.speech.held());
    assert!(matches!(app.voice.phase, VoicePhase::Listening { .. }));
    // A while with nothing said: the quiet is let go.
    TALK.with(|t| t.set(None));
    OVER.with(|o| o.set(None));
    let long_ago = std::time::Duration::from_secs_f32(crate::app::voice::NO_SPEECH_TIMEOUT + 1.0);
    app.voice.phase = VoicePhase::Listening {
        since: std::time::Instant::now() - long_ago,
        level: 0.0,
    };
    voice_tick(&mut app);
    assert!(
        matches!(app.voice.phase, VoicePhase::Listening { since, .. } if since.elapsed() < long_ago),
        "listening afresh: {:?}",
        app.voice.phase
    );
    assert_eq!(OPENED.with(|o| o.get()), opened, "never opened anew");
}

/// What is heard over Clew that is only Clew's own words, heard back —
/// echo the canceller left — lets it go on where it was held.
#[test]
fn clews_own_words_heard_back_let_it_go_on() {
    let (mut app, output) = talked_over("speech-talk-echo", "the total adds every price");
    talk(Some(0.1));
    voice_tick(&mut app);
    assert!(app.voice.speech.held());
    talk(Some(crate::app::voice::END_SILENCE + 0.1));
    voice_tick(&mut app);
    assert!(app.voice.speech.is_speaking(), "it goes on");
    assert!(!app.voice.speech.held());
    assert!(!output.lock().unwrap().paused, "faded back in");
    assert!(app.proj.ask_turns.len() == 1, "nothing asked");
}

/// Held, and what was said never heard — the microphone gone quiet — Clew
/// goes on by itself.
#[test]
fn a_hold_never_answered_lets_clew_go_on() {
    let (mut app, output) = talked_over("speech-talk-timeout", "wait");
    talk(Some(0.1));
    voice_tick(&mut app);
    assert!(app.voice.speech.held());
    app.voice
        .speech
        .hold_since_for_test(std::time::Duration::from_secs(60));
    let task = app.update(Message::Voice(VoiceMsg::SpeakTick));
    settle(&mut app, task);
    assert!(!app.voice.speech.held());
    assert!(!output.lock().unwrap().paused);
}

/// Talking over Clew turned off: the microphone is opened as it was, with
/// nothing taken out, and waits while Clew speaks.
#[test]
fn not_to_be_talked_over_the_microphone_waits() {
    let (mut app, _voice, _output) = speech_app("speech-talk-off");
    let model = app.voice.prefs.model();
    let store = app.voice.store.clone().unwrap();
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.path(model), b"a model").unwrap();
    app.voice.open_mic = talking_mic;
    app.ask_view = AskView::Compact;
    let _ = app.update(Message::Voice(VoiceMsg::SetTalkOver(false)));
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(!CANCELLED.with(|c| c.get()));
    asked(&mut app, 1, true);
    delta(&mut app, 1, "The total adds every price.");
    assert_eq!(app.voice.phase, VoicePhase::Idle, "waits for Clew");
}

/// Not to be talked over, Clew waits for what the reader was saying as it
/// began; once it is said, the microphone waits for Clew — closed, Clew
/// heard through it as it is, with nothing taken out.
#[test]
fn not_to_be_talked_over_the_microphone_waits_once_the_reader_has_said_it() {
    let (mut app, _voice, _output) = speech_app("speech-talk-off-said");
    let model = app.voice.prefs.model();
    let store = app.voice.store.clone().unwrap();
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.path(model), b"a model").unwrap();
    app.voice.open_mic = talking_mic;
    app.voice.transcriber = Arc::new(Hears("the total adds every price".into()));
    app.ask_view = AskView::Compact;
    let _ = app.update(Message::Voice(VoiceMsg::SetTalkOver(false)));
    TALK.with(|t| t.set(None));
    OVER.with(|o| o.set(None));
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    // The reader is talking as Clew begins: it is held for them.
    talk(Some(0.1));
    asked(&mut app, 1, true);
    delta(
        &mut app,
        1,
        "The total adds every price. Then the discount is taken off.",
    );
    voice_tick(&mut app);
    assert!(app.voice.speech.held(), "{:?}", app.voice.phase);
    talk(Some(crate::app::voice::END_SILENCE + 0.1));
    voice_tick(&mut app);
    assert!(app.voice.speech.is_speaking(), "it goes on");
    assert_eq!(app.voice.phase, VoicePhase::Idle, "the microphone waits");
}

/// Words said over Clew are the reader's but for a few nearly all Clew's
/// own; anything short is the reader's, whatever Clew said.
#[test]
fn what_is_said_over_clew_is_told_from_its_echo() {
    use crate::app::voice::said_over;
    let said = vec!["The total adds every price.".to_string()];
    assert!(said_over("stop", &said), "short: the reader's");
    // Short, though Clew said it too: still the reader's.
    let stop = vec!["You can stop the server first.".to_string()];
    assert!(said_over("stop", &stop));
    assert!(said_over("stop the", &stop));
    assert!(!said_over("you can stop the server", &stop));
    assert!(said_over("wait wait", &said));
    assert!(!said_over("The total adds every", &said), "Clew's own");
    assert!(said_over("open the cart file please", &said));
    assert!(!said_over("", &said), "nothing said");
    assert!(!said_over("…", &said), "no words");
    // Without spaces: each character a word.
    let chinese = vec!["总价把每个价格加起来。".to_string()];
    assert!(!said_over("每个价格加起来", &chinese));
    assert!(said_over("打开购物车文件", &chinese));
}
