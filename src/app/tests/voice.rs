//! Voice (`app::voice`): the voice key, listening — on a test's microphone,
//! which hears what the test says it hears — the words read as one of the
//! window's commands or as a question for Ask, and the speech models.

use super::*;
use crate::app::voice::{
    END_SILENCE, HeardOutcome, Listening, NO_SPEECH_TIMEOUT, Transcriber, VoicePhase, VoicePrefs,
};
use clew_voice::capture::Recording;
use clew_voice::whisper::Options;
use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

/// What the test's microphone reports, shared with the test.
#[derive(Default)]
struct MicScript {
    level: f32,
    silence: Option<f32>,
    full: bool,
    error: Option<String>,
    stopped: bool,
}

struct FakeMic(Arc<Mutex<MicScript>>);

impl Listening for FakeMic {
    fn level(&self) -> f32 {
        self.0.lock().unwrap().level
    }
    fn silence_after_speech(&self) -> Option<f32> {
        self.0.lock().unwrap().silence
    }
    fn recent(&self, _seconds: f32) -> (Vec<f32>, u32) {
        (spoken(), 16_000)
    }
    fn full(&self) -> bool {
        self.0.lock().unwrap().full
    }
    fn error(&self) -> Option<String> {
        self.0.lock().unwrap().error.clone()
    }
    fn stop(self: Box<Self>) -> Recording {
        self.0.lock().unwrap().stopped = true;
        Recording {
            samples: spoken(),
            rate: 16_000,
        }
    }
}

thread_local! {
    /// The microphone the next listen opens; none: there is no microphone.
    static MIC: RefCell<Option<Arc<Mutex<MicScript>>>> = const { RefCell::new(None) };
}

fn fake_mic(_max_seconds: u32, _cancel_echo: bool) -> Result<Box<dyn Listening>, String> {
    MIC.with(|m| m.borrow().clone())
        .map(|script| Box::new(FakeMic(script)) as Box<dyn Listening>)
        .ok_or_else(|| "no microphone is available".to_string())
}

/// A second of something like a voice after a second of quiet, at 16 kHz.
fn spoken() -> Vec<f32> {
    (0..32_000)
        .map(|i| {
            if i < 16_000 {
                0.0
            } else {
                let t = i as f32 / 16_000.0;
                0.1 * (2.0 * std::f32::consts::PI * 180.0 * t).sin()
            }
        })
        .collect()
}

/// A recognizer that hears `words`, whatever was said, and keeps what it
/// was given.
struct Hears {
    words: String,
    hints: Mutex<Vec<String>>,
    heard_audio: Mutex<usize>,
    /// The model file it was last asked to hear with.
    model: Mutex<Option<PathBuf>>,
}

impl Transcriber for Hears {
    fn transcribe(
        &self,
        model: &Path,
        audio: &[f32],
        options: &Options,
        cancel: &AtomicBool,
    ) -> Result<String, String> {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        *self.model.lock().unwrap() = Some(model.to_path_buf());
        self.hints.lock().unwrap().push(options.hint.clone());
        *self.heard_audio.lock().unwrap() = audio.len();
        Ok(self.words.clone())
    }
}

/// A recognizer that hears each of its words in turn.
struct InTurn(Mutex<std::collections::VecDeque<String>>);

impl Transcriber for InTurn {
    fn transcribe(
        &self,
        _model: &Path,
        _audio: &[f32],
        _options: &Options,
        _cancel: &AtomicBool,
    ) -> Result<String, String> {
        Ok(self.0.lock().unwrap().pop_front().unwrap_or_default())
    }
}

/// An app with a project open, a model "downloaded", the test's microphone,
/// and a recognizer that hears `words`.
fn voice_app(tag: &str, words: &str) -> (App, Arc<Mutex<MicScript>>, Arc<Hears>) {
    let mut app = scanned_app(tag);
    let store = own_store(&mut app, tag);
    let model = app.voice.prefs.model();
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.path(model), b"a model").unwrap();
    let script = Arc::new(Mutex::new(MicScript::default()));
    MIC.with(|m| *m.borrow_mut() = Some(script.clone()));
    app.voice.open_mic = fake_mic;
    let hears = Arc::new(Hears {
        words: words.into(),
        hints: Mutex::new(Vec::new()),
        heard_audio: Mutex::new(0),
        model: Mutex::new(None),
    });
    app.voice.transcriber = hears.clone();
    (app, script, hears)
}

/// A models folder of the test's own — the data directory is shared — and
/// the settings as they start out.
fn own_store(app: &mut App, tag: &str) -> clew_voice::models::Store {
    let dir = test_dir(&format!("{tag}-models"));
    let _ = std::fs::remove_dir_all(&dir);
    let store = clew_voice::models::Store::new(dir);
    app.voice.store = Some(store.clone());
    app.voice.prefs = VoicePrefs::default();
    store
}

/// Turn the microphone on, hear a pause after speech, and feed back what
/// was heard — worked out out of sight, the microphone listening on.
fn say(app: &mut App, script: &Arc<Mutex<MicScript>>) {
    app.voice.reset_for_test();
    script.lock().unwrap().silence = None;
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(app.voice.is_listening(), "{}", app.status);
    script.lock().unwrap().silence = Some(END_SILENCE + 0.1);
    let task = app.update(Message::Voice(VoiceMsg::Tick));
    assert!(app.voice.transcribing(), "what was said is worked out");
    assert!(script.lock().unwrap().stopped, "what was heard was taken");
    assert!(app.voice.is_listening(), "and the microphone listens on");
    // The microphone open afresh has heard nothing yet.
    script.lock().unwrap().silence = None;
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert!(!app.voice.transcribing());
}

#[test]
fn a_command_heard_runs_as_the_palette_runs_it() {
    let (mut app, script, hears) = voice_app("voice-command", "Open the call graph.");
    say(&mut app, &script);
    assert!(app.voice.is_listening(), "it listens on for the next");
    assert_eq!(app.proj.overlay, Some(Overlay::ProjectCalls));
    assert_eq!(
        app.voice.heard.as_ref().map(|h| &h.outcome),
        Some(&HeardOutcome::Ran("Call Graph".into()))
    );
    assert!(app.status.contains("Call Graph"), "{}", app.status);
    // Whisper was told the window's command names, to spell them so, and
    // given the speech without the quiet before it.
    let hints = hears.hints.lock().unwrap();
    assert!(hints[0].contains("Call Graph"), "{}", hints[0]);
    assert!(!hints[0].contains('…'));
    let audio = *hears.heard_audio.lock().unwrap();
    assert!(audio < spoken().len(), "trimmed: {audio}");
    assert!(
        app.palette_recent
            .first()
            .is_some_and(|t| t == "Call Graph")
    );
}

/// A command that costs something waits for a yes — listened for, the
/// microphone on; Esc is a no, and the microphone listens on.
#[test]
fn a_costly_command_asks_first() {
    let (mut app, script, _) = voice_app("voice-costly", "explain all");
    say(&mut app, &script);
    assert_eq!(
        app.voice.confirm.as_ref().map(|w| w.said()),
        Some("Explain All")
    );
    assert!(!app.settings.open, "nothing ran yet");
    assert!(app.voice.is_listening(), "for the yes");
    assert!(app.status.contains("Explain All?"), "{}", app.status);
    assert!(app.voice.takes_esc());
    // Esc: no.
    let _ = app.update(key_press(
        keyboard::Key::Named(keyboard::key::Named::Escape),
        keyboard::Modifiers::empty(),
    ));
    assert!(app.voice.confirm.is_none());
    assert!(!app.settings.open);
    assert_eq!(app.status, "Didn't run Explain All");
    assert!(
        app.voice.is_listening(),
        "Esc is no to it, not to listening"
    );
    assert!(!app.voice.takes_esc(), "Esc is the window's again");
    // Again, and yes: it runs (with no key to pay for it, Settings asks
    // for one).
    say(&mut app, &script);
    assert_eq!(
        app.voice.confirm.as_ref().map(|w| w.said()),
        Some("Explain All")
    );
    let _ = app.update(Message::Voice(VoiceMsg::Confirm));
    assert!(app.voice.confirm.is_none());
    assert!(app.settings.open, "the pass asked for a key");
    assert_eq!(
        app.voice.heard.as_ref().map(|h| &h.outcome),
        Some(&HeardOutcome::Ran("Explain All".into()))
    );
}

/// What is not a command is a question: it goes to Ask, which opens on it.
#[test]
fn a_question_heard_goes_to_ask() {
    let (mut app, script, _) = voice_app("voice-question", "Where is the origin defined?");
    app.voice.prefs.send_questions = false;
    say(&mut app, &script);
    assert_eq!(app.ask_view, AskView::Compact);
    assert_eq!(app.proj.ask_input, "Where is the origin defined?");
    assert_eq!(
        app.voice.heard.as_ref().map(|h| &h.outcome),
        Some(&HeardOutcome::Asked)
    );
    assert!(
        app.proj.ask_turns.is_empty(),
        "not sent: the setting says not"
    );
    // What waits in the input was said, and its turn will say so; typing
    // in it makes it the reader's own words.
    assert!(app.proj.ask_spoken, "the input holds what was said");
    let typed = format!("{} Briefly.", app.proj.ask_input);
    let _ = app.update(Message::Ask(AskMsg::InputChanged(typed)));
    assert!(!app.proj.ask_spoken, "typed over, it is typed");
    // Asked at once when the setting says so — here with no model to ask,
    // so the question waits in the input, said why. Asked so, it does not
    // open the card over the code.
    let (mut app, script, _) = voice_app("voice-question-send", "where is the origin defined");
    say(&mut app, &script);
    assert!(!app.ask_view.is_open(), "{:?}", app.ask_view);
    assert_eq!(app.proj.ask_input, "where is the origin defined");
    assert!(!app.status.starts_with("Asking"), "{}", app.status);
    // A command the user turned off running: also a question.
    let (mut app, script, _) = voice_app("voice-commands-off", "open the call graph");
    app.voice.prefs.run_commands = false;
    app.voice.prefs.send_questions = false;
    say(&mut app, &script);
    assert_eq!(app.proj.overlay, None);
    assert_eq!(app.proj.ask_input, "open the call graph");
}

/// A question said aloud is asked without the card opening over the code
/// (someone finding their way by voice does not want it over what they
/// went to): it is answered where Ask is, and an answer that comes with the
/// card put away is marked on the pill until the card is opened.
#[test]
fn a_question_said_is_answered_without_opening_the_card() {
    let (mut app, script, _) = voice_app("voice-question-hidden", "where is the origin defined");
    app.llm_config_cache = Some(Some(unreachable_llm()));
    // Grounded on a pinned line, it is asked with no index.
    let root = app.proj.project.as_ref().unwrap().root.clone();
    app.proj.ask_pins.push(crate::app::model::AskPin {
        rel: "src/lib.rs".into(),
        file: root.join("src/lib.rs"),
        line: 1,
        code: "pub struct Point;".into(),
    });
    say(&mut app, &script);
    assert!(app.proj.asking, "asked: {}", app.status);
    assert!(!app.ask_view.is_open(), "{:?}", app.ask_view);
    assert!(!app.proj.ask_unread);
    // Its answer comes with the card put away: marked, until it is opened.
    app.proj.asking = false;
    app.proj.ask_turns.push(crate::app::model::AskTurn {
        stream: 7,
        question: "where is the origin defined".into(),
        spoken: true,
        streaming: true,
        ..Default::default()
    });
    let _ = app.on_ask_stream_ended(7, clew_protocol::StreamOutcome::Done);
    assert!(app.proj.ask_unread);
    let _ = app.update(Message::Ask(AskMsg::Open));
    assert!(app.ask_view.is_open());
    assert!(!app.proj.ask_unread, "read once the card is open");
    // Answered with the card open, nothing is marked.
    app.proj.ask_turns.push(crate::app::model::AskTurn {
        stream: 8,
        question: "and the end?".into(),
        streaming: true,
        ..Default::default()
    });
    let _ = app.on_ask_stream_ended(8, clew_protocol::StreamOutcome::Done);
    assert!(!app.proj.ask_unread);
}

/// The microphone, turned on, listens until it is turned off: a quiet that
/// goes on is let go and listened past, not reported as a microphone that
/// hears nothing. Turned off before anything is said, nothing comes of it;
/// turned off while someone is heard, what they said is still acted on.
#[test]
fn the_microphone_listens_until_it_is_turned_off() {
    let (mut app, script, _) = voice_app("voice-mic-on", "open the call graph");
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(app.voice.mic_on);
    assert!(app.voice.is_listening(), "{}", app.status);
    assert_eq!(app.status, "Listening… say a command or a question");
    // Nothing said for long: listened past, quietly, afresh.
    if let VoicePhase::Listening { since, .. } = &mut app.voice.phase {
        *since -= std::time::Duration::from_secs_f32(NO_SPEECH_TIMEOUT + 1.0);
    }
    let _ = app.update(Message::Voice(VoiceMsg::Tick));
    assert!(
        matches!(app.voice.phase, VoicePhase::Listening { since, .. }
            if since.elapsed().as_secs_f32() < NO_SPEECH_TIMEOUT),
        "{:?}",
        app.voice.phase
    );
    assert_eq!(app.status, "Listening… say a command or a question");
    assert!(app.voice.heard.is_none(), "nothing said, nothing shown");
    // Esc, with no question waiting and Clew quiet, is not the
    // microphone's: it listens on.
    let _ = app.update(key_press(
        keyboard::Key::Named(keyboard::key::Named::Escape),
        keyboard::Modifiers::empty(),
    ));
    assert!(app.voice.is_listening());
    // Turned off before anything is said: nothing comes of it.
    app.voice.forget_last_press();
    script.lock().unwrap().stopped = false;
    let task = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(!app.voice.mic_on);
    assert_eq!(app.voice.phase, VoicePhase::Idle);
    assert!(script.lock().unwrap().stopped, "the microphone is closed");
    assert!(!app.voice.transcribing());
    assert_eq!(task.units(), 0);
    assert_eq!(app.status, "Stopped listening");
    // Turned off as someone is heard: the press ends what they say, as a
    // pause would, and it is acted on — the microphone stays off.
    app.voice.forget_last_press();
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    script.lock().unwrap().silence = Some(0.2);
    app.voice.forget_last_press();
    let task = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(!app.voice.mic_on);
    assert!(app.voice.transcribing());
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert_eq!(app.proj.overlay, Some(Overlay::ProjectCalls));
    assert_eq!(app.voice.phase, VoicePhase::Idle, "it stays off");
}

/// What is said while what was said before is still being worked out
/// waits its turn: each is worked out, and acted on, in the order said —
/// none dropped for the next.
#[test]
fn what_is_said_is_worked_out_in_order() {
    let (mut app, script, _) = voice_app("voice-in-order", "");
    let heard = Arc::new(InTurn(Mutex::new(
        ["open the call graph", "where is the origin defined"]
            .map(String::from)
            .into(),
    )));
    app.voice.transcriber = heard.clone();
    app.voice.prefs.send_questions = false;
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    script.lock().unwrap().silence = Some(END_SILENCE + 0.1);
    let first = app.update(Message::Voice(VoiceMsg::Tick));
    assert!(app.voice.is_listening(), "listening on at once");
    // Said again before the first is worked out.
    let second = app.update(Message::Voice(VoiceMsg::Tick));
    assert_eq!(second.units(), 0, "it waits for the first");
    assert!(app.voice.is_listening());
    script.lock().unwrap().silence = None;
    for message in run_task(first) {
        for message in run_task(app.update(message)) {
            let _ = app.update(message);
        }
    }
    assert_eq!(app.proj.overlay, Some(Overlay::ProjectCalls), "the first");
    assert_eq!(
        app.proj.ask_input, "where is the origin defined",
        "then the next"
    );
    assert!(heard.0.lock().unwrap().is_empty());
    assert!(!app.voice.transcribing());
    assert!(app.voice.is_listening());
}

/// Not to be talked over, the microphone, on, waits while Clew reads an
/// answer aloud — it would hear Clew — and listens again once it is over,
/// whatever ended it; an answer that is not read aloud is no reason to
/// wait. Put away to its pill, the conversation is listened for all the
/// same.
#[test]
fn the_microphone_waits_while_an_answer_is_read_aloud() {
    use crate::app::voice::ReadAloud;
    use clew_protocol::StreamOutcome;
    let (mut app, _script, _) = voice_app("voice-waits-for-answer", "");
    app.voice.prefs.talk_over = false;
    app.ask_view = AskView::Compact;
    let turn = |app: &mut App, stream: u64, spoken: bool| {
        app.proj.ask_turns.push(crate::app::model::AskTurn {
            stream,
            question: "where is it?".into(),
            spoken,
            streaming: true,
            ..Default::default()
        });
    };
    // Off: the answer ends, the microphone stays shut.
    turn(&mut app, 1, true);
    let _ = app.on_ask_stream_ended(1, StreamOutcome::Done);
    assert_eq!(app.voice.phase, VoicePhase::Idle);
    // On: a spoken question's answer, read aloud, is waited for.
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(app.voice.is_listening());
    turn(&mut app, 2, true);
    let _ = app.update(Message::Voice(VoiceMsg::Tick));
    assert_eq!(app.voice.phase, VoicePhase::Idle, "it waits for the answer");
    assert!(app.voice.mic_on, "still on");
    let _ = app.on_ask_stream_ended(2, StreamOutcome::Done);
    assert!(app.voice.is_listening(), "{:?}", app.voice.phase);
    // A Stop, too: the microphone is on.
    turn(&mut app, 3, true);
    let _ = app.update(Message::Voice(VoiceMsg::Tick));
    assert_eq!(app.voice.phase, VoicePhase::Idle);
    let _ = app.on_ask_stream_ended(3, StreamOutcome::Stopped);
    assert!(app.voice.is_listening());
    // A typed question's answer is not read aloud: nothing to wait for —
    // unless every answer is.
    turn(&mut app, 4, false);
    let _ = app.update(Message::Voice(VoiceMsg::Tick));
    assert!(app.voice.is_listening());
    let _ = app.on_ask_stream_ended(4, StreamOutcome::Done);
    app.voice.prefs.read_aloud = ReadAloud::Always;
    turn(&mut app, 5, false);
    let _ = app.update(Message::Voice(VoiceMsg::Tick));
    assert_eq!(app.voice.phase, VoicePhase::Idle);
    let _ = app.on_ask_stream_ended(5, StreamOutcome::Done);
    assert!(app.voice.is_listening());
    // None read aloud: none waited for.
    app.voice.prefs.read_aloud = ReadAloud::Never;
    turn(&mut app, 6, true);
    let _ = app.update(Message::Voice(VoiceMsg::Tick));
    assert!(app.voice.is_listening());
    let _ = app.on_ask_stream_ended(6, StreamOutcome::Done);
    // Put away to its pill, it listens all the same.
    app.voice.prefs.read_aloud = ReadAloud::Spoken;
    app.ask_view = AskView::Hidden;
    turn(&mut app, 7, true);
    let _ = app.update(Message::Voice(VoiceMsg::Tick));
    let _ = app.on_ask_stream_ended(7, StreamOutcome::Done);
    assert!(app.voice.is_listening(), "{:?}", app.voice.phase);
}

/// Nothing that could be read is let go without a word — no "didn't catch
/// that": the microphone listens on. A microphone that goes away turns it
/// off, saying why.
#[test]
fn silence_is_let_go_and_a_lost_microphone_says_so() {
    let (mut app, script, _) = voice_app("voice-empty", "");
    say(&mut app, &script);
    assert!(app.voice.is_listening());
    assert_eq!(app.voice.heard, None);
    assert_eq!(app.status, "Listening… say a command or a question");
    assert!(app.proj.ask_input.is_empty());
    assert!(!app.ask_view.is_open(), "nothing heard opens no card");
    // A repeat of the key, ignored.
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(app.voice.is_listening(), "a repeat of the key, ignored");
    // The microphone going away mid-way.
    script.lock().unwrap().error = Some("the microphone is not available".into());
    let _ = app.update(Message::Voice(VoiceMsg::Tick));
    assert_eq!(app.voice.phase, VoicePhase::Idle);
    assert!(!app.voice.mic_on);
    assert!(app.status.contains("not available"), "{}", app.status);
}

/// The voice key: ⇧⌘S, rebindable — and a menu item, "Talk".
#[test]
fn the_voice_key_starts_listening() {
    let (mut app, _script, _) = voice_app("voice-key", "zoom in");
    let _ = app.update(key_press(
        keyboard::Key::Character("s".into()),
        keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT,
    ));
    assert!(app.voice.is_listening());
    assert_eq!(keymap::Action::Voice.menu().title, "Talk");
    assert!(
        app.palette_commands().iter().any(|c| c.title == "Talk"),
        "and in the palette"
    );
}

/// No model yet: the key asks for one, in Settings, rather than listening.
#[test]
fn without_a_model_voice_asks_for_one() {
    let mut app = scanned_app("voice-no-model");
    own_store(&mut app, "voice-no-model");
    app.voice.open_mic = fake_mic;
    MIC.with(|m| *m.borrow_mut() = Some(Arc::new(Mutex::new(MicScript::default()))));
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(!app.voice.is_listening());
    assert!(app.settings.open);
    assert!(app.status.contains("download"), "{}", app.status);
    // No microphone at all: said, and nothing listens.
    let (mut app, _, _) = voice_app("voice-no-mic", "zoom in");
    MIC.with(|m| *m.borrow_mut() = None);
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(!app.voice.is_listening());
    assert!(app.status.contains("no microphone"), "{}", app.status);
}

fn junk_fetch(
    _url: &str,
    max_bytes: u64,
    _cancel: &AtomicBool,
    sink: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    assert!(max_bytes > 100_000_000, "the model's own cap");
    std::thread::sleep(std::time::Duration::from_millis(200));
    sink.write_all(b"not the model")
        .map_err(|e| e.to_string())?;
    progress(13, Some(13));
    Ok(13)
}

fn failing_fetch(
    url: &str,
    _max_bytes: u64,
    _cancel: &AtomicBool,
    _sink: &mut dyn std::io::Write,
    _progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    assert!(url.starts_with("https://huggingface.co/ggerganov/whisper.cpp/"));
    Err("HTTP 403 Forbidden".into())
}

/// Gets a megabyte and a half of a model, then loses the connection — the
/// error naming the signed address a CDN served it from, as Clew's own
/// transport does.
fn dropping_fetch(
    _url: &str,
    _max_bytes: u64,
    _cancel: &AtomicBool,
    sink: &mut dyn std::io::Write,
    _progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    sink.write_all(&vec![0u8; 1_500_000])
        .map_err(|e| e.to_string())?;
    Err(
        "https://us.aws.cdn.hf.co/xet-bridge-us/641ab5/32b578?Expires=1793944821\
         &Policy=eyJTdGF0ZW1lbnQiOlt7&Signature=MEUCIFr4ga: the connection closed before \
         all bytes were read (1500000 of 147951465)"
            .into(),
    )
}

/// A download that fails part-way says how far it got and why, in words —
/// not the address it failed at — and what it said goes once Settings,
/// where it is said, is closed.
#[test]
fn a_failed_download_says_why_briefly_until_settings_closes() {
    let mut app = scanned_app("voice-download-dropped");
    own_store(&mut app, "voice-download-dropped");
    let _ = app.update(Message::Settings(SettingsMsg::Open));
    app.voice.fetch = dropping_fetch;
    let task = app.update(Message::Voice(VoiceMsg::Download("base.en")));
    for message in run_task(task) {
        let _ = app.update(message);
    }
    let size = clew_voice::models::find("base.en").unwrap().size_mb;
    let error = app.voice.download_errors.get("base.en").cloned().unwrap();
    assert_eq!(
        error,
        format!("Stopped at 1 of {size} MB: the connection closed")
    );
    assert!(!app.status.contains("https://"), "{}", app.status);
    let _ = app.update(Message::Settings(SettingsMsg::Close));
    assert!(
        app.voice.download_errors.is_empty(),
        "closing Settings keeps it"
    );
}

/// A failed download says why in the reader's language, how far it got
/// in those words too, and the address it failed at cut to its host — a
/// proxy it went through still said.
#[test]
fn a_failed_download_says_why_in_the_readers_language() {
    use crate::app::voice::download_failure;
    use crate::i18n::{Lang, with_lang};
    let model = clew_voice::models::find("base.en").unwrap();
    let signed = "https://us.aws.cdn.hf.co/xet-bridge-us/641ab5/32b578?Expires=1793944821\
                  &Signature=MEUCIFr4ga";
    with_lang(Lang::Fr, || {
        assert_eq!(
            download_failure(
                &format!("{signed}: the transfer took too long — giving up"),
                704_000_000,
                model
            ),
            format!(
                "Interrompu à 704 sur {} Mo\u{a0}: us.aws.cdn.hf.co\u{a0}: le transfert a \
                 pris trop de temps — abandon",
                model.size_mb
            )
        );
        let said = download_failure(
            &format!("{signed} (through the proxy http://p:3128): connection refused"),
            0,
            model,
        );
        assert!(
            said.starts_with("us.aws.cdn.hf.co (via le proxy http://p:3128)\u{a0}: "),
            "{said}"
        );
    });
    assert_eq!(
        download_failure(&format!("{signed}: HTTP 404 Not Found"), 0, model),
        "us.aws.cdn.hf.co: HTTP 404 Not Found"
    );
    assert_eq!(
        download_failure("the download was closed", 0, model),
        "the download was closed"
    );
}

/// A download that is not the model as published is refused and leaves
/// nothing behind; one that fails says why; Cancel stops one.
#[test]
fn model_downloads_are_verified() {
    let mut app = scanned_app("voice-download");
    own_store(&mut app, "voice-download");
    let model = clew_voice::models::find("base.en").unwrap();
    app.voice.fetch = junk_fetch;
    let task = app.update(Message::Voice(VoiceMsg::Download("base.en")));
    assert!(app.voice.downloads.contains_key("base.en"));
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert!(app.voice.downloads.is_empty());
    let error = app.voice.download_errors.get("base.en").cloned().unwrap();
    assert!(
        error.contains("does not match the published ggml-base.en.bin"),
        "{error}"
    );
    assert!(!app.voice.installed(model));
    let store = app.voice.store.clone().unwrap();
    assert_eq!(std::fs::read_dir(store.dir()).unwrap().count(), 0);
    app.voice.fetch = failing_fetch;
    let task = app.update(Message::Voice(VoiceMsg::Download("base.en")));
    assert!(app.voice.download_errors.is_empty(), "a new try clears it");
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert!(app.status.contains("403"), "{}", app.status);
    // Cancelled: whatever it says after is nobody's.
    app.voice.fetch = junk_fetch;
    let task = app.update(Message::Voice(VoiceMsg::Download("base.en")));
    let _ = app.update(Message::Voice(VoiceMsg::CancelDownload("base.en")));
    assert!(app.voice.downloads.is_empty());
    let before = app.status.clone();
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert_eq!(app.status, before, "a cancelled download says nothing");
    // One of Echo's, whose exact size is published: a download of another
    // size is refused for that before its hash is looked at.
    let task = app.update(Message::Voice(VoiceMsg::Download("small-q5_1")));
    for message in run_task(task) {
        let _ = app.update(message);
    }
    let error = app
        .voice
        .download_errors
        .get("small-q5_1")
        .cloned()
        .unwrap();
    assert!(error.contains("13 bytes, not 190085487"), "{error}");
    // Removing a model that is there.
    std::fs::write(store.path(model), b"x").unwrap();
    assert!(app.voice.installed(model));
    let _ = app.update(Message::Voice(VoiceMsg::RemoveModel("base.en")));
    assert!(!app.voice.installed(model));
}

/// A model downloaded is the one the voice key hears with when the one
/// chosen is not on this computer — and that is kept; one chosen and here
/// stays chosen.
#[test]
fn a_downloaded_model_is_the_one_the_voice_key_uses() {
    // A data directory of its own: config.toml is everyone's.
    let data = test_dir("voice-downloaded-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let (mut app, script, _) = voice_app("voice-downloaded", "hello");
    let store = app.voice.store.clone().unwrap();
    let small = app.voice.prefs.model();
    assert_eq!(small.id, "small-q5_1", "the first one chosen");
    std::fs::remove_file(store.path(small)).unwrap();
    // Tiny is downloaded (the transfer itself is left out).
    app.voice.fetch = junk_fetch;
    let _ = app.update(Message::Voice(VoiceMsg::Download("tiny.en")));
    let tiny = clew_voice::models::find("tiny.en").unwrap();
    std::fs::write(store.path(tiny), b"a model").unwrap();
    let _ = app.update(Message::Voice(VoiceMsg::Downloaded {
        id: "tiny.en",
        result: Ok(store.path(tiny)),
    }));
    assert!(app.status.contains("is ready"), "{}", app.status);
    assert_eq!(app.voice.prefs.model, "tiny.en");
    assert_eq!(VoicePrefs::load().model, "tiny.en", "kept");
    // One chosen and here stays chosen when another finishes downloading
    // (picked while it downloaded, say): that one is only said to be here.
    let _ = app.update(Message::Voice(VoiceMsg::Download("base.en")));
    let base = clew_voice::models::find("base.en").unwrap();
    std::fs::write(store.path(base), b"a model").unwrap();
    let _ = app.update(Message::Voice(VoiceMsg::Downloaded {
        id: "base.en",
        result: Ok(store.path(base)),
    }));
    assert_eq!(app.voice.prefs.model, "tiny.en");
    assert!(app.status.contains("is downloaded"), "{}", app.status);
    // And the voice key listens with it, as the status says.
    app.voice.reset_for_test();
    script.lock().unwrap().silence = None;
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(app.voice.is_listening(), "{}", app.status);
    assert!(!app.settings.open);
}

/// The model the voice key hears with, removed while another is on this
/// computer: that one is used, not the removed one asked for again. With
/// none left, the choice stays (Settings offers it to download).
#[test]
fn removing_the_chosen_model_uses_one_still_here() {
    let data = test_dir("voice-removed-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let (mut app, script, _) = voice_app("voice-removed", "hello");
    let store = app.voice.store.clone().unwrap();
    let small = app.voice.prefs.model();
    assert!(app.voice.installed(small));
    let tiny = clew_voice::models::find("tiny.en").unwrap();
    std::fs::write(store.path(tiny), b"a model").unwrap();
    let _ = app.update(Message::Voice(VoiceMsg::RemoveModel("small-q5_1")));
    assert!(!app.voice.installed(small));
    assert_eq!(app.voice.prefs.model, "tiny.en");
    assert_eq!(VoicePrefs::load().model, "tiny.en", "kept");
    app.voice.reset_for_test();
    script.lock().unwrap().silence = None;
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(app.voice.is_listening(), "{}", app.status);
    // Removing one not chosen leaves the choice alone.
    std::fs::write(store.path(small), b"a model").unwrap();
    let _ = app.update(Message::Voice(VoiceMsg::RemoveModel("small-q5_1")));
    assert_eq!(app.voice.prefs.model, "tiny.en");
    // The last one gone: still chosen, to download again.
    let _ = app.update(Message::Voice(VoiceMsg::RemoveModel("tiny.en")));
    assert_eq!(app.voice.prefs.model, "tiny.en");
}

/// The voice settings are kept in config.toml, and read back.
#[test]
fn voice_settings_are_kept() {
    // A data directory of its own: config.toml is everyone's.
    let data = test_dir("voice-prefs-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let mut app = scanned_app("voice-prefs");
    assert_eq!(app.voice.prefs, VoicePrefs::default(), "none kept yet");
    let _ = app.update(Message::Voice(VoiceMsg::PickModel("large-v3-turbo-q5_0")));
    let _ = app.update(Message::Voice(VoiceMsg::SetLanguage(Some("de"))));
    let _ = app.update(Message::Voice(VoiceMsg::SetRunCommands(false)));
    let _ = app.update(Message::Voice(VoiceMsg::SetSendQuestions(false)));
    let read = VoicePrefs::load();
    assert_eq!(read, app.voice.prefs);
    assert_eq!(read.model, "large-v3-turbo-q5_0");
    assert_eq!(read.language.as_deref(), Some("de"));
    assert!(!read.run_commands && !read.send_questions);
    // An unknown model is not taken.
    let _ = app.update(Message::Voice(VoiceMsg::PickModel("huge")));
    assert_eq!(app.voice.prefs.model, "large-v3-turbo-q5_0");
    let _ = app.update(Message::Voice(VoiceMsg::SetLanguage(None)));
    assert_eq!(VoicePrefs::load().language, None);
}

/// A command said in the reader's language, as the palette shows it: with
/// or without its category, the words that only say "do it" left out —
/// and nothing more, nothing that puts away what it would show.
#[test]
fn a_command_is_heard_in_the_language_the_palette_shows() {
    use crate::app::voice::said_as_shown;
    use crate::i18n::Lang;
    let zh = [
        "调用图",
        "视图：隐藏缩略图",
        "视图：显示缩略图",
        "打开文件夹…",
    ];
    assert_eq!(said_as_shown(Lang::Zh, "打开调用图", &zh), Some(0));
    assert_eq!(said_as_shown(Lang::Zh, "请打开调用图。", &zh), Some(0));
    assert_eq!(said_as_shown(Lang::Zh, "显示缩略图", &zh), Some(2));
    assert_eq!(said_as_shown(Lang::Zh, "隐藏缩略图", &zh), Some(1));
    assert_eq!(said_as_shown(Lang::Zh, "视图：隐藏缩略图", &zh), Some(1));
    assert_eq!(said_as_shown(Lang::Zh, "打开文件夹", &zh), Some(3));
    assert_eq!(
        said_as_shown(Lang::Zh, "关闭缩略图", &zh),
        None,
        "close never shows it"
    );
    assert_eq!(
        said_as_shown(Lang::Zh, "解释一下调用图", &zh),
        None,
        "a request, for Ask"
    );
    let fr = [
        "Graphe d’appels",
        "Affichage : Masquer la minimap",
        "Atteindre la ligne…",
    ];
    assert_eq!(
        said_as_shown(Lang::Fr, "Ouvre le graphe d'appels.", &fr),
        Some(0)
    );
    assert_eq!(said_as_shown(Lang::Fr, "Masquer la minimap", &fr), Some(1));
    // Said formally, or to a group: the longer word is the one left out
    // ("ouvrez", not "ouvre" and a "z").
    assert_eq!(
        said_as_shown(Lang::Fr, "Ouvrez le graphe d'appels", &fr),
        Some(0)
    );
    assert_eq!(
        said_as_shown(
            Lang::Fr,
            "Affichez le graphe d'appels, s'il vous plaît",
            &fr
        ),
        Some(0)
    );
    assert_eq!(
        said_as_shown(Lang::Fr, "Allez aux paramètres", &["Paramètres"]),
        Some(0)
    );
    assert_eq!(
        said_as_shown(
            Lang::Fr,
            "Ouvrez une nouvelle fenêtre",
            &["Nouvelle fenêtre"]
        ),
        Some(0)
    );
    assert_eq!(said_as_shown(Lang::Fr, "Atteindre la ligne", &fr), Some(2));
    assert_eq!(
        said_as_shown(Lang::Fr, "Pourquoi le graphe d'appels ?", &fr),
        None
    );
    assert_eq!(
        said_as_shown(Lang::Ko, "호출 그래프 열어 줘", &["호출 그래프"]),
        Some(0)
    );
    assert_eq!(
        said_as_shown(Lang::Ko, "호출 그래프를 보여주세요", &["호출 그래프"]),
        Some(0)
    );
    assert_eq!(
        said_as_shown(Lang::Ja, "コールグラフを開いてください", &["コールグラフ"]),
        Some(0)
    );
    assert_eq!(
        said_as_shown(Lang::Ja, "コールグラフ", &["コールグラフ", "コールグラフ"]),
        None,
        "two fit: neither runs"
    );
    assert_eq!(
        said_as_shown(Lang::En, "Call Graph", &["Call Graph"]),
        None,
        "English is the word matcher's"
    );
}

/// The voice panel's command examples, in each language, run the commands
/// they name: each catalog names a command as its palette shows it.
#[test]
fn each_languages_examples_run_what_they_say() {
    use crate::i18n::{Lang, lookup, with_lang};
    use crate::ui::TRY_SAYING;
    for lang in Lang::ALL {
        with_lang(lang, || {
            let words = |example: &str| {
                lookup(example)
                    .trim_matches(['“', '”', '「', '」', '«', '»', ' ', '\u{a0}'])
                    .to_string()
            };
            let (mut app, script, _) = voice_app(
                &format!("voice-example-{}", lang.code()),
                &words(TRY_SAYING[0].0),
            );
            say(&mut app, &script);
            assert_eq!(
                app.voice.heard.as_ref().map(|h| &h.outcome),
                Some(&HeardOutcome::Ran("Call Graph".into())),
                "{}: {:?}",
                lang.code(),
                words(TRY_SAYING[0].0)
            );
            let (mut app, script, _) = voice_app(
                &format!("voice-example-debug-{}", lang.code()),
                &words(TRY_SAYING[4].0),
            );
            say(&mut app, &script);
            assert_eq!(
                app.voice.heard.as_ref().map(|h| &h.outcome),
                Some(&HeardOutcome::Confirm("Start Debugging".into())),
                "{}: {:?}",
                lang.code(),
                words(TRY_SAYING[4].0)
            );
            // Hiding the minimap: what the palette offers while it shows.
            app.show_minimap = true;
            let commands = app.palette_commands();
            let shown: Vec<&str> = commands.iter().map(|c| c.shown()).collect();
            let titles: Vec<&str> = commands.iter().map(|c| c.title.as_str()).collect();
            let said = words(TRY_SAYING[1].0);
            let found = clew_voice::intent::command(&said, &titles)
                .or_else(|| crate::app::voice::said_as_shown(lang, &said, &shown));
            assert_eq!(
                found.map(|i| titles[i]),
                Some("View: Hide Minimap"),
                "{}: {said:?}",
                lang.code()
            );
        });
    }
}

/// The recognizer hears `words` from now on.
fn hears(app: &mut App, words: &str) {
    app.voice.transcriber = Arc::new(Hears {
        words: words.into(),
        hints: Mutex::new(Vec::new()),
        heard_audio: Mutex::new(0),
        model: Mutex::new(None),
    });
}

/// Say `words`, and land the file a pane then waits for, as the runtime
/// would.
fn say_words(app: &mut App, script: &Arc<Mutex<MicScript>>, words: &str) {
    hears(app, words);
    say(app, script);
    let pane = app.proj.active;
    if let Some(req) = app.proj.link.pane_pending[pane] {
        let open = app.proj.link.pane_opening[pane]
            .clone()
            .filter(|o| o.req == req)
            .expect("the pane's open is kept");
        land_active_load(app, open.abs, open.target);
    }
}

/// The caret's line in the active pane, 1-based.
fn caret_line(app: &App) -> Option<usize> {
    app.active_viewer()?.caret.map(|(line, _)| line + 1)
}

/// What was done with what was heard.
fn ran(app: &App) -> Option<&str> {
    match &app.voice.heard.as_ref()?.outcome {
        HeardOutcome::Ran(done) => Some(done),
        _ => None,
    }
}

/// A line said is gone to — in figures or in words — and a line said with no
/// file open goes to Ask, which can say there is none.
#[test]
fn a_line_said_is_gone_to() {
    let (mut app, script, _) = voice_app("voice-line", "");
    say_words(&mut app, &script, "Go to line 4.");
    assert!(
        app.voice
            .heard
            .as_ref()
            .is_some_and(|h| h.outcome == HeardOutcome::Asked),
        "no file is open"
    );
    open_synchronously(&mut app, "src/lib.rs", None);
    say_words(&mut app, &script, "Go to line 4.");
    assert_eq!(caret_line(&app), Some(4), "{}", app.status);
    assert_eq!(ran(&app), Some("Line 4"));
    say_words(&mut app, &script, "line two");
    assert_eq!(caret_line(&app), Some(2));
    // Past the end: the last line.
    say_words(&mut app, &script, "go to line 900");
    assert_eq!(
        caret_line(&app),
        Some(app.active_viewer().unwrap().lines.len())
    );
    // The palette's own "Go to Line…" asks for one; said without a number,
    // that is what runs.
    say_words(&mut app, &script, "Go to line");
    assert_eq!(ran(&app), Some("Go to Line…"));
}

/// A long file is moved through by voice as by its keys: by a screen, by
/// lines, to either end; and folded and unfolded.
#[test]
fn a_file_is_scrolled_and_folded_by_voice() {
    let (mut app, script, _) = voice_app("voice-scroll", "");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let body: String = (0..120)
        .map(|i| format!("fn f{i}() {{\n    let x = {i};\n}}\n"))
        .collect();
    std::fs::write(root.join("src/long.rs"), body).unwrap();
    scan_synchronously(&mut app, root);
    open_synchronously(&mut app, "src/long.rs", None);
    let line_height = app.line_height();
    app.active_viewer_mut().unwrap().viewport_h = 20.0 * line_height;
    let scroll = |app: &App| app.active_viewer().unwrap().scroll_y;

    say_words(&mut app, &script, "Scroll down.");
    assert_eq!(ran(&app), Some("Scroll Down"), "{}", app.status);
    assert_eq!(scroll(&app), 19.0 * line_height, "a screen, less a line");
    assert_eq!(caret_line(&app), Some(20), "the caret goes as far");
    say_words(&mut app, &script, "scroll down 10 lines");
    assert_eq!(scroll(&app), 29.0 * line_height);
    say_words(&mut app, &script, "Scroll up five lines.");
    assert_eq!(scroll(&app), 24.0 * line_height);
    say_words(&mut app, &script, "scroll up");
    assert_eq!(scroll(&app), 5.0 * line_height);

    // Near the end, down is still down: on until the last line is the
    // first in view, as the wheel goes.
    let rows = app.active_viewer().unwrap().content_rows() as f32;
    app.active_viewer_mut().unwrap().scroll_y = (rows - 10.0) * line_height;
    say_words(&mut app, &script, "scroll down");
    assert_eq!(scroll(&app), (rows - 1.0) * line_height);
    say_words(&mut app, &script, "Go to the end of the file.");
    assert_eq!(ran(&app), Some("Go to End of File"));
    assert_eq!(
        caret_line(&app),
        Some(app.active_viewer().unwrap().lines.len())
    );
    say_words(&mut app, &script, "go to top of file");
    assert_eq!(caret_line(&app), Some(1));

    say_words(&mut app, &script, "Fold all.");
    assert_eq!(ran(&app), Some("Fold All"));
    assert!(!app.active_viewer().unwrap().collapsed.is_empty());
    say_words(&mut app, &script, "unfold all");
    assert!(app.active_viewer().unwrap().collapsed.is_empty());
}

/// The debugger by voice: a breakpoint on a line said, the program stepped
/// with no yes for each step, a watch added and taken away; starting and
/// stopping it still asks — and the yes, or the no, can be said.
#[test]
fn the_debugger_is_driven_by_voice() {
    let (mut app, script, _) = voice_app("voice-debug", "");
    open_synchronously(&mut app, "src/lib.rs", None);
    let abs = app.active_viewer().unwrap().abs.clone();
    say_words(&mut app, &script, "Breakpoint on line 4.");
    assert_eq!(ran(&app), Some("Breakpoint on line 4"), "{}", app.status);
    assert!(
        app.debug
            .breakpoints
            .get(&abs)
            .is_some_and(|b| b.contains_key(&4))
    );
    // No line 90: nothing set, and said so.
    say_words(&mut app, &script, "Breakpoint on line 90.");
    assert!(
        !app.debug
            .breakpoints
            .get(&abs)
            .is_some_and(|b| b.contains_key(&90))
    );
    assert!(app.status.contains("no line 90"), "{}", app.status);
    assert!(
        matches!(
            app.voice.heard.as_ref().map(|h| &h.outcome),
            Some(HeardOutcome::Failed(_))
        ),
        "not shown as done"
    );
    say_words(&mut app, &script, "toggle breakpoint on line four");
    assert!(
        !app.debug
            .breakpoints
            .get(&abs)
            .is_some_and(|b| b.contains_key(&4))
    );

    // Starting asks; "no" says no, "yes" runs it.
    say_words(&mut app, &script, "Start debugging.");
    assert_eq!(
        app.voice.confirm.as_ref().map(|w| w.said()),
        Some("Start Debugging")
    );
    say_words(&mut app, &script, "No, never mind.");
    assert!(app.voice.confirm.is_none());
    assert!(app.status.contains("Start Debugging"), "{}", app.status);
    say_words(&mut app, &script, "Start debugging.");
    say_words(&mut app, &script, "Yes, run it.");
    assert!(app.voice.confirm.is_none());
    assert_eq!(ran(&app), Some("Start Debugging"));
    // Something else said instead puts the question away and is read as
    // itself.
    say_words(&mut app, &script, "Start debugging.");
    say_words(&mut app, &script, "go to line 2");
    assert!(app.voice.confirm.is_none());
    assert_eq!(caret_line(&app), Some(2));

    // A program stopped: each step runs as said.
    app.debug.session = Some(DebugSession {
        client: None,
        status: DebugStatus::Stopped,
        thread_id: Some(1),
        frames: Vec::new(),
        scopes: Vec::new(),
        watches: Vec::new(),
        output: Vec::new(),
        current: None,
        program: PathBuf::from("/bin/true"),
        args: Vec::new(),
        cwd: PathBuf::from("/"),
        addr: None,
    });
    for (said, title) in [
        ("Step over.", "Debug: Step Over"),
        ("step into", "Debug: Step Into"),
        ("Step out", "Debug: Step Out"),
        ("continue", "Debug: Continue"),
    ] {
        say_words(&mut app, &script, said);
        assert_eq!(ran(&app), Some(title), "{said}: {}", app.status);
        assert!(app.voice.confirm.is_none(), "{said}");
    }
    say_words(&mut app, &script, "Stop debugging.");
    assert_eq!(
        app.voice.confirm.as_ref().map(|w| w.said()),
        Some("Debug: Stop")
    );
    say_words(&mut app, &script, "cancel");
    assert!(app.voice.confirm.is_none());

    say_words(&mut app, &script, "Watch cart total.");
    assert_eq!(app.debug.watches, ["cart total"]);
    say_words(&mut app, &script, "stop watching cart total");
    assert!(app.debug.watches.is_empty());
    say_words(&mut app, &script, "Turn on hover evaluation.");
    assert_eq!(
        app.voice.confirm.as_ref().map(|w| w.said()),
        Some("Debug: Turn On Hover Evaluation"),
        "evaluating in the program as the mouse moves asks"
    );
}

/// Words to look for, a file and a symbol said by name, Settings' pages and
/// the find bar's matches: each where it is said to be.
#[test]
fn what_is_named_is_found_and_opened() {
    let (mut app, script, _) = voice_app("voice-named", "");
    // The project's symbols, indexed as the runtime does in a task.
    let files = app.proj.project.as_ref().unwrap().files.clone();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let indexed = app.update(Message::Watch(WatchMsg::SymbolIndexDone {
        stamp: Stamp {
            root: Some(root.clone()),
            epoch: app.project_epoch,
            conn: None,
        },
        indexed: index::build_indexed(&root, files),
    }));
    settle_imports(&mut app, indexed);
    say_words(&mut app, &script, "Open notes dot txt.");
    assert_eq!(
        app.active_viewer().map(|v| v.rel.as_str()),
        Some("notes.txt"),
        "{}",
        app.status
    );
    // An extension misheard: the file of that name.
    say_words(&mut app, &script, "Open, lib.ide,");
    assert_eq!(
        app.active_viewer().map(|v| v.rel.as_str()),
        Some("src/lib.rs"),
        "{}",
        app.status
    );
    say_words(&mut app, &script, "Go to origin.");
    assert_eq!(ran(&app), Some("origin"), "{}", app.status);
    let v = app.active_viewer().unwrap();
    assert_eq!(v.rel, "src/lib.rs");
    assert_eq!(caret_line(&app), Some(3));

    say_words(&mut app, &script, "Find point.");
    assert!(app.proj.find.open);
    assert_eq!(app.proj.find.query, "point");
    assert_eq!(app.proj.find.matches.len(), 3);
    say_words(&mut app, &script, "find next");
    assert_eq!(app.proj.find.current, 1, "{}", app.status);
    say_words(&mut app, &script, "find previous");
    assert_eq!(app.proj.find.current, 0);
    say_words(&mut app, &script, "close find");
    assert!(!app.proj.find.open);

    // What is not a file's name, nor words to find, is a question.
    for question in [
        "Open it.",
        "Show me.",
        "Go to the end.",
        "Find where the origin is worked out.",
    ] {
        say_words(&mut app, &script, question);
        assert!(
            app.voice
                .heard
                .as_ref()
                .is_some_and(|h| h.outcome == HeardOutcome::Asked),
            "{question}: {}",
            app.status
        );
        app.proj.ask_input.clear();
    }

    say_words(&mut app, &script, "Search for needle.");
    assert_eq!(app.proj.search.query, "needle");
    assert_eq!(app.sidebar, SidebarTab::Search);

    say_words(&mut app, &script, "Open the voice settings.");
    assert!(app.settings.open, "{}", app.status);
    assert_eq!(app.settings.page, SettingsPage::Voice);
}

/// The phrases are heard in the reader's language too; whisper is told the
/// titles, not them.
#[test]
fn phrases_are_heard_in_the_readers_language() {
    use crate::i18n::{Lang, with_lang};
    with_lang(Lang::Fr, || {
        let (mut app, script, hears) = voice_app("voice-phrase-fr", "");
        open_synchronously(&mut app, "src/lib.rs", None);
        say_words(&mut app, &script, "Aller à la ligne 4.");
        assert_eq!(caret_line(&app), Some(4), "{}", app.status);
        drop(hears);
        // A title beginning as a phrase does is the title: "Rechercher les
        // références" is Find References, not a search for "les références".
        app.active_viewer_mut().unwrap().caret = Some((2, 7));
        say_words(&mut app, &script, "Rechercher les références");
        assert_eq!(ran(&app), Some("Find References"), "{}", app.status);
        assert!(app.proj.search.query.is_empty());
        say_words(&mut app, &script, "Démarrer le débogage");
        assert!(
            matches!(
                &app.voice.confirm,
                Some(crate::app::voice::Waiting::Command(t)) if t == "Start Debugging"
            ),
            "{:?}",
            app.voice.confirm
        );
        say_words(&mut app, &script, "Oui, vas-y.");
        assert_eq!(ran(&app), Some("Start Debugging"), "{}", app.status);
    });
    with_lang(Lang::Zh, || {
        let (mut app, script, _) = voice_app("voice-phrase-zh", "");
        open_synchronously(&mut app, "src/lib.rs", None);
        say_words(&mut app, &script, "跳到第3行。");
        assert_eq!(caret_line(&app), Some(3), "{}", app.status);
        say_words(&mut app, &script, "在第2行设置断点");
        let abs = app.active_viewer().unwrap().abs.clone();
        assert!(
            app.debug
                .breakpoints
                .get(&abs)
                .is_some_and(|b| b.contains_key(&2)),
            "{}",
            app.status
        );
    });
    let (mut app, script, hears) = voice_app("voice-phrase-hint", "go to line 2");
    open_synchronously(&mut app, "src/lib.rs", None);
    say(&mut app, &script);
    let hints = hears.hints.lock().unwrap();
    // The titles, and nothing that whisper might hear in what was not
    // clearly said.
    assert!(hints[0].contains("Scroll Down"), "{}", hints[0]);
    assert!(!hints[0].contains("breakpoint on line"), "{}", hints[0]);
    assert!(
        !hints[0].split(", ").any(|w| w == "Talk"),
        "the voice key's own command: {}",
        hints[0]
    );
}

/// Every command the window offers runs when its name is said, in each
/// language — with a file open, its find bar up and a program stopped in
/// the debugger, so that every command is offered.
#[test]
fn every_command_runs_when_its_name_is_said() {
    use crate::i18n::{Lang, with_lang};
    let (mut app, _script, _) = voice_app("voice-every-command", "");
    open_synchronously(&mut app, "src/lib.rs", None);
    app.active_viewer_mut().unwrap().caret = Some((2, 7));
    let _ = app.update(Message::Editor(EditorMsg::FindOpened));
    app.debug.session = Some(DebugSession {
        client: None,
        status: DebugStatus::Stopped,
        thread_id: Some(1),
        frames: Vec::new(),
        scopes: Vec::new(),
        watches: Vec::new(),
        output: Vec::new(),
        current: None,
        program: PathBuf::from("/bin/true"),
        args: Vec::new(),
        cwd: PathBuf::from("/"),
        addr: None,
    });
    for lang in Lang::ALL {
        with_lang(lang, || {
            let commands = app.palette_commands();
            let titles: Vec<&str> = commands.iter().map(|c| c.title.as_str()).collect();
            let shown: Vec<&str> = commands.iter().map(|c| c.shown()).collect();
            let mut unheard = Vec::new();
            for (i, command) in commands.iter().enumerate() {
                let said = command.shown().trim_end_matches('…');
                let found = clew_voice::intent::command(said, &titles)
                    .or_else(|| crate::app::voice::said_as_shown(lang, said, &shown));
                if found.map(|f| titles[f]) != Some(titles[i]) {
                    unheard.push(format!("{said:?} → {:?}", found.map(|f| titles[f])));
                }
            }
            assert!(unheard.is_empty(), "{}: {unheard:#?}", lang.code());
        });
    }
    // And as the README says them.
    let commands = app.palette_commands();
    let titles: Vec<&str> = commands.iter().map(|c| c.title.as_str()).collect();
    for (said, title) in [
        ("scroll down", "Scroll Down"),
        ("go to the end of the file", "Go to End of File"),
        ("fold all", "Fold All"),
        ("unfold all", "Unfold All"),
        ("fold here", "Toggle Fold Here"),
        ("find next", "Find Next"),
        ("find previous", "Find Previous"),
        ("close find", "Find: Close"),
        ("step over", "Debug: Step Over"),
        ("continue", "Debug: Continue"),
        ("voice settings", "Settings: Voice"),
        ("full screen", "Window: Full Screen"),
        ("maximize", "Window: Toggle Maximize"),
        ("maximize the window", "Window: Toggle Maximize"),
        // The sidebar's tabs, by their names: the reader's own notes, said
        // as theirs, apart from the margin's.
        ("my notes", "Sidebar: Your Notes"),
        ("show my notes", "Sidebar: Your Notes"),
        // The caret's own command, of the same name, unless the sidebar is
        // said.
        ("call hierarchy", "Call Hierarchy"),
        ("sidebar call hierarchy", "Sidebar: Call Hierarchy"),
        ("margin explanations", "Margin Explanations"),
        ("margin calls", "Margin Calls"),
        ("margin history", "Margin History"),
    ] {
        assert_eq!(
            clew_voice::intent::command(said, &titles).map(|i| titles[i]),
            Some(title),
            "{said:?}"
        );
    }
}

/// What is said is heard with the model listening began with: one another
/// window picks meanwhile — not downloaded, say — is the next listen's.
#[test]
fn listening_keeps_the_model_it_began_with() {
    let (mut app, script, hears) = voice_app("voice-model-kept", "hello");
    let began = app.voice.prefs.model();
    let store = app.voice.store.clone().unwrap();
    app.voice.reset_for_test();
    script.lock().unwrap().silence = None;
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(app.voice.is_listening(), "{}", app.status);
    // Another window's pick, handed to this one by the shell.
    app.voice.prefs.model = "tiny.en".into();
    script.lock().unwrap().silence = Some(END_SILENCE + 0.1);
    let task = app.update(Message::Voice(VoiceMsg::Tick));
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert_eq!(
        hears.model.lock().unwrap().as_deref(),
        Some(store.path(began).as_path())
    );
}

/// A model downloading is not downloaded again in another window — both
/// wrote one partial file, and the first to finish moved the other's
/// half-written bytes into place — and, chosen, it stays chosen when
/// another one finishes first.
#[test]
fn a_model_on_its_way_is_neither_downloaded_twice_nor_passed_over() {
    let data = test_dir("voice-coming-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let (mut app, _script, _) = voice_app("voice-coming", "hello");
    let store = app.voice.store.clone().unwrap();
    let small = app.voice.prefs.model();
    std::fs::remove_file(store.path(small)).unwrap();
    app.voice.fetch = junk_fetch;
    // The chosen model, on its way in this window (its transfer not run).
    let small_transfer = app.update(Message::Voice(VoiceMsg::Download(small.id)));
    assert!(app.voice.downloads.contains_key(small.id));
    let mut other = blank_app();
    other.voice.store = Some(store.clone());
    other.voice.fetch = junk_fetch;
    let _ = other.update(Message::Voice(VoiceMsg::Download(small.id)));
    assert!(other.voice.downloads.is_empty(), "downloaded twice");
    assert!(
        other.status.contains("already downloading"),
        "{}",
        other.status
    );
    // Tiny, finished first, does not take its place.
    let _tiny_transfer = app.update(Message::Voice(VoiceMsg::Download("tiny.en")));
    let tiny = clew_voice::models::find("tiny.en").unwrap();
    std::fs::write(store.path(tiny), b"a model").unwrap();
    let _ = app.update(Message::Voice(VoiceMsg::Downloaded {
        id: "tiny.en",
        result: Ok(store.path(tiny)),
    }));
    assert_eq!(app.voice.prefs.model, small.id);
    assert!(app.status.contains("is downloaded"), "{}", app.status);
    // Its transfer over, another window may download it.
    drop(small_transfer);
    let _ = app.update(Message::Voice(VoiceMsg::CancelDownload(small.id)));
    let _ = other.update(Message::Voice(VoiceMsg::Download(small.id)));
    assert!(
        other.voice.downloads.contains_key(small.id),
        "{}",
        other.status
    );
}

// ---- Smart Turn: when the speaker has finished ---------------------------

/// A judge that answers what the test says it will, in order, and keeps
/// how much audio each judgement was given.
#[derive(Default)]
struct FakeJudge {
    answers: Mutex<Vec<Result<f32, String>>>,
    asked: Mutex<Vec<usize>>,
    warmed: Mutex<usize>,
    stopped: Mutex<usize>,
}

impl crate::app::turn::Judging for FakeJudge {
    fn warm(&self, _file: &std::path::Path) -> Result<(), String> {
        *self.warmed.lock().unwrap() += 1;
        Ok(())
    }
    fn judge(&self, _file: &std::path::Path, samples: &[f32]) -> Result<f32, String> {
        self.asked.lock().unwrap().push(samples.len());
        let mut answers = self.answers.lock().unwrap();
        if answers.is_empty() {
            Err("not answered".into())
        } else {
            answers.remove(0)
        }
    }
    fn stop(&self) {
        *self.stopped.lock().unwrap() += 1;
    }
}

/// [`voice_app`] with Smart Turn: its model "downloaded" (a file of its
/// size), judged by a judge that answers `answers`; and listening begun.
fn turn_app(
    tag: &str,
    answers: Vec<Result<f32, String>>,
    model: bool,
) -> (App, Arc<Mutex<MicScript>>, Arc<FakeJudge>) {
    let (mut app, script, _) = voice_app(tag, "open settings");
    if model {
        let store = app.voice.store.clone().unwrap();
        std::fs::create_dir_all(clew_voice::turn::dir(&store)).unwrap();
        std::fs::File::create(clew_voice::turn::path(&store))
            .unwrap()
            .set_len(clew_voice::turn::MODEL.bytes)
            .unwrap();
    }
    let judge = Arc::new(FakeJudge {
        answers: Mutex::new(answers),
        ..FakeJudge::default()
    });
    app.voice.turn.judges = Some(judge.clone());
    app.voice.reset_for_test();
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(app.voice.is_listening(), "{}", app.status);
    (app, script, judge)
}

/// The microphone has been quiet `quiet` seconds since speech: a tick.
fn quiet_for(app: &mut App, script: &Arc<Mutex<MicScript>>, quiet: f32) -> Task<Message> {
    script.lock().unwrap().silence = Some(quiet);
    app.update(Message::Voice(VoiceMsg::Tick))
}

/// Whether the turn was ended: what was said is being worked out.
fn transcribing(app: &App) -> bool {
    app.voice.transcribing()
}

/// A turn that sounds finished ends listening at its first short pause,
/// long before the pause listening waits out without the model; the model
/// hears the last of what was said.
#[test]
fn a_finished_turn_ends_listening_at_its_first_pause() {
    use crate::app::turn::JUDGE_AFTER;
    let (mut app, script, judge) = turn_app("turn-finished", vec![Ok(0.9)], true);
    assert!(
        quiet_for(&mut app, &script, 0.1).units() == 0,
        "too short to judge"
    );
    assert!(judge.asked.lock().unwrap().is_empty());
    let task = quiet_for(&mut app, &script, JUDGE_AFTER + 0.05);
    assert!(app.voice.is_listening(), "listening while it is judged");
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert_eq!(*judge.asked.lock().unwrap(), [spoken().len()]);
    assert!(transcribing(&app), "a finished turn ends listening");
    assert!(script.lock().unwrap().stopped);
    const { assert!(JUDGE_AFTER < END_SILENCE) };
}

/// A turn that sounds unfinished — a pause to think — is waited through,
/// past the pause that ends listening without the model, up to its own.
#[test]
fn an_unfinished_turn_is_waited_through() {
    use crate::app::turn::{HOLD_FOR, JUDGE_AFTER};
    let (mut app, script, _) = turn_app("turn-unfinished", vec![Ok(0.1)], true);
    let task = quiet_for(&mut app, &script, JUDGE_AFTER + 0.05);
    for message in run_task(task) {
        let _ = app.update(message);
    }
    let _ = quiet_for(&mut app, &script, END_SILENCE + 0.1);
    assert!(app.voice.is_listening(), "an unfinished turn is waited for");
    let _ = quiet_for(&mut app, &script, HOLD_FOR - 0.1);
    assert!(app.voice.is_listening());
    let _ = quiet_for(&mut app, &script, HOLD_FOR);
    assert!(transcribing(&app), "and not for ever");
}

/// Speech coming back is a pause of its own when it stops again: judged
/// afresh. A judgement of the pause before it, landing late, changes
/// nothing.
#[test]
fn each_pause_is_judged_afresh() {
    use crate::app::turn::JUDGE_AFTER;
    let (mut app, script, judge) = turn_app("turn-afresh", vec![Ok(0.95), Ok(0.1), Ok(0.9)], true);
    // A pause judged finished — but speech comes back before the answer.
    let late = quiet_for(&mut app, &script, JUDGE_AFTER + 0.05);
    let _ = quiet_for(&mut app, &script, 0.02);
    for message in run_task(late) {
        let _ = app.update(message);
    }
    assert!(app.voice.is_listening(), "a judgement of a pause gone by");
    // The next pause: unfinished, then speech, then a finished one.
    let task = quiet_for(&mut app, &script, JUDGE_AFTER + 0.05);
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert!(app.voice.is_listening());
    let _ = quiet_for(&mut app, &script, 0.02);
    let task = quiet_for(&mut app, &script, JUDGE_AFTER + 0.05);
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert_eq!(judge.asked.lock().unwrap().len(), 3, "once a pause");
    assert!(transcribing(&app));
}

/// A pause is judged once, however many ticks it lasts.
#[test]
fn a_pause_is_judged_once() {
    use crate::app::turn::JUDGE_AFTER;
    let (mut app, script, judge) = turn_app("turn-once", vec![Ok(0.2)], true);
    let first = quiet_for(&mut app, &script, JUDGE_AFTER + 0.01);
    let second = quiet_for(&mut app, &script, JUDGE_AFTER + 0.07);
    assert_eq!(second.units(), 0, "not asked again while it is judged");
    for message in run_task(first) {
        let _ = app.update(message);
    }
    let third = quiet_for(&mut app, &script, JUDGE_AFTER + 0.2);
    assert_eq!(third.units(), 0, "nor once it has been");
    assert_eq!(judge.asked.lock().unwrap().len(), 1);
}

/// Without the model, or when it cannot answer, a pause ends listening as
/// it always has; what went wrong is kept for Settings to show.
#[test]
fn without_a_judgement_a_pause_ends_as_before() {
    use crate::app::turn::JUDGE_AFTER;
    // Not downloaded: nothing is asked.
    let (mut app, script, judge) = turn_app("turn-none", vec![Ok(0.9)], false);
    assert_eq!(quiet_for(&mut app, &script, JUDGE_AFTER + 0.05).units(), 0);
    assert!(app.voice.is_listening());
    let _ = quiet_for(&mut app, &script, END_SILENCE + 0.01);
    assert!(transcribing(&app));
    assert!(judge.asked.lock().unwrap().is_empty());
    assert_eq!(*judge.warmed.lock().unwrap(), 0, "no model to load");
    // It fails: the pause is waited out as without it.
    let (mut app, script, _) = turn_app("turn-fails", vec![Err("it broke".into())], true);
    let task = quiet_for(&mut app, &script, JUDGE_AFTER + 0.05);
    for message in run_task(task) {
        let _ = app.update(message);
    }
    let _ = quiet_for(&mut app, &script, END_SILENCE - 0.1);
    assert!(app.voice.is_listening());
    let _ = quiet_for(&mut app, &script, END_SILENCE + 0.01);
    assert!(transcribing(&app));
    assert_eq!(app.voice.turn.error.as_deref(), Some("it broke"));
    // No `clew-speak` to run it: as without the model.
    let (mut app, script, _) = turn_app("turn-no-program", vec![Ok(0.9)], true);
    app.voice.turn.judges = None;
    assert_eq!(quiet_for(&mut app, &script, JUDGE_AFTER + 0.05).units(), 0);
    assert!(app.voice.is_listening());
}

/// Listening loads the model meanwhile, so the first pause is judged at
/// once.
#[test]
fn listening_loads_the_model() {
    let (_app, _script, judge) = turn_app("turn-warm", Vec::new(), true);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while *judge.warmed.lock().unwrap() == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(*judge.warmed.lock().unwrap(), 1);
}

/// A download of Smart Turn that is not it, held to its own cap.
fn junk_turn_fetch(
    _url: &str,
    max_bytes: u64,
    _cancel: &AtomicBool,
    sink: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    assert_eq!(
        max_bytes,
        clew_voice::turn::MODEL.bytes,
        "the model's own cap"
    );
    std::thread::sleep(std::time::Duration::from_millis(200));
    sink.write_all(b"not the model")
        .map_err(|e| e.to_string())?;
    progress(13, Some(13));
    Ok(13)
}

/// Smart Turn is downloaded checked against its size and checksum, and
/// removed with what runs it let go first.
#[test]
fn smart_turn_downloads_verified_and_is_removed() {
    let (mut app, _script, judge) = turn_app("turn-download", Vec::new(), false);
    let store = app.voice.store.clone().unwrap();
    app.voice.fetch = junk_turn_fetch;
    let task = app.update(Message::Voice(VoiceMsg::DownloadTurn));
    assert!(app.voice.turn.download.is_some());
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert!(app.voice.turn.download.is_none());
    let error = app.voice.turn.error.clone().unwrap();
    assert!(
        error.contains("does not match the published smart-turn-v3.2-cpu.onnx"),
        "{error}"
    );
    assert!(!clew_voice::turn::installed(&store));
    // Cancelled: whatever it says after is nobody's.
    let task = app.update(Message::Voice(VoiceMsg::DownloadTurn));
    assert!(app.voice.turn.error.is_none(), "a new try clears it");
    let _ = app.update(Message::Voice(VoiceMsg::CancelTurnDownload));
    let before = app.status.clone();
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert_eq!(app.status, before, "a cancelled download says nothing");
    // Removed: the model let go of first.
    std::fs::create_dir_all(clew_voice::turn::dir(&store)).unwrap();
    std::fs::File::create(clew_voice::turn::path(&store))
        .unwrap()
        .set_len(clew_voice::turn::MODEL.bytes)
        .unwrap();
    assert!(clew_voice::turn::installed(&store));
    let task = app.update(Message::Voice(VoiceMsg::RemoveTurn));
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert!(!clew_voice::turn::installed(&store));
    assert_eq!(*judge.stopped.lock().unwrap(), 1);
    assert!(app.status.contains("removed"), "{}", app.status);
}

/// Spoken again after a pause was sent to be judged, before any tick has
/// seen it: the answer that the pause finished a turn is about a pause that
/// is over, and listening goes on.
#[test]
fn speech_back_before_the_answer_keeps_listening() {
    use crate::app::turn::JUDGE_AFTER;
    let (mut app, script, _) = turn_app("turn-spoke-again", vec![Ok(0.95)], true);
    let task = quiet_for(&mut app, &script, JUDGE_AFTER + 0.05);
    script.lock().unwrap().silence = Some(0.03);
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert!(app.voice.is_listening(), "cut off mid-word");
}

/// A failed judgement is said under the model until one succeeds; one
/// from a listening already over is not said at all.
#[test]
fn a_judgement_failure_is_said_until_one_succeeds() {
    use crate::app::turn::JUDGE_AFTER;
    let (mut app, script, _) = turn_app(
        "turn-fails-once",
        vec![Err("took too long".into()), Ok(0.2)],
        true,
    );
    for message in run_task(quiet_for(&mut app, &script, JUDGE_AFTER + 0.05)) {
        let _ = app.update(message);
    }
    assert_eq!(app.voice.turn.error.as_deref(), Some("took too long"));
    let _ = quiet_for(&mut app, &script, 0.02);
    for message in run_task(quiet_for(&mut app, &script, JUDGE_AFTER + 0.05)) {
        let _ = app.update(message);
    }
    assert_eq!(app.voice.turn.error, None, "it answers again");
    // An answer for the listening before this one.
    let listen = app.voice.turn.pause.listen;
    let _ = app.update(Message::Voice(VoiceMsg::Judged {
        listen: listen - 1,
        pause: 0,
        result: Err("long ago".into()),
    }));
    assert_eq!(app.voice.turn.error, None);
}

/// A model being removed — in any window — judges nothing: a pause ends
/// as without it, whether the removal began before listening or during it.
#[test]
fn a_model_being_removed_judges_nothing() {
    use crate::app::turn::JUDGE_AFTER;
    use crate::app::voice::{Claim, ClaimedFor};
    let (mut app, script, judge) = turn_app("turn-removing", vec![Ok(0.9)], true);
    let store = app.voice.store.clone().unwrap();
    let removal = Claim::take(clew_voice::turn::path(&store), ClaimedFor::Removal).unwrap();
    assert_eq!(quiet_for(&mut app, &script, JUDGE_AFTER + 0.05).units(), 0);
    let _ = quiet_for(&mut app, &script, END_SILENCE + 0.01);
    assert!(transcribing(&app));
    // Begun before listening: not even loaded.
    app.voice.reset_for_test();
    script.lock().unwrap().silence = None;
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(app.turn_model().is_none());
    drop(removal);
    assert!(app.turn_model().is_some(), "here again once it is let go");
    assert!(judge.asked.lock().unwrap().is_empty());
}

/// L10: the voice key with the model chosen not on this computer says so,
/// and opens Settings → Voice: that there is no model at all only when
/// there is none — another one downloaded, it says which is missing and
/// that one that is here can be chosen.
#[test]
fn the_voice_key_says_which_model_is_missing() {
    let mut app = blank_app();
    let store = own_store(&mut app, "voice-key-missing");
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(!app.voice.is_listening());
    assert!(
        app.status.starts_with("Voice needs a speech model"),
        "{}",
        app.status
    );
    assert_eq!(app.settings.page, SettingsPage::Voice);
    let _ = app.update(Message::Settings(SettingsMsg::Close));
    // Base downloaded, Small still the one chosen.
    let base = clew_voice::models::find("base.en").unwrap();
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.path(base), b"x").unwrap();
    app.voice.reset_for_test();
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(!app.voice.is_listening());
    assert_eq!(
        app.status,
        "Small is not downloaded yet — download it (190 MB) in Settings → Voice, or choose \
         one that is"
    );
    assert!(app.settings.open);
    assert_eq!(app.settings.page, SettingsPage::Voice);
}

/// The sentence that found nothing: the interface's language changed as
/// it is said — its name misheard — by the command the palette and the
/// agent run too. (Made, it would change every test's language: what it
/// makes is checked, not made.)
#[test]
fn the_apps_language_is_changed_as_it_is_said() {
    use crate::i18n::{Lang, LangPref};
    let app = blank_app();
    let settings = app.settings_choices();
    for said in [
        "change the app's language to frensh",
        "Change the app's language to French.",
        "switch to French",
    ] {
        let (s, c) = crate::app::choices::chosen(said, &settings).expect(said);
        let choice = &settings[s].choices[c];
        assert_eq!(choice.title, "Language: French", "{said:?}");
        assert!(
            matches!(
                choice.message,
                Message::Settings(SettingsMsg::SetLanguage(LangPref::Chosen(Lang::Fr)))
            ),
            "{said:?}: {:?}",
            choice.message
        );
    }
    // The palette lists it, for the keyboard and for the agent.
    let palette = app.palette_commands();
    let french = palette
        .iter()
        .find(|c| c.title == "Language: French")
        .expect("the palette offers the language");
    assert!(matches!(
        french.action,
        crate::app::palette::PaletteAction::Send(Message::Settings(SettingsMsg::SetLanguage(
            LangPref::Chosen(Lang::Fr)
        )))
    ));
    assert!(!french.costly(), "the agent runs it without asking");
}

/// A setting said is changed — the window's own, here, as the language
/// is every window's — and a switch is turned off and on by its name.
#[test]
fn a_setting_said_is_changed() {
    use crate::app::voice::ReadAloud;
    let (mut app, script, _) = voice_app("voice-setting", "");
    assert_eq!(app.voice.prefs.read_aloud, ReadAloud::Spoken);
    say_words(&mut app, &script, "Turn off read answers aloud.");
    assert_eq!(
        app.voice.prefs.read_aloud,
        ReadAloud::Never,
        "{}",
        app.status
    );
    assert_eq!(ran(&app), Some("Read answers aloud: Never"));
    say_words(&mut app, &script, "turn read answers aloud back on");
    assert_eq!(
        app.voice.prefs.read_aloud,
        ReadAloud::Always,
        "{}",
        app.status
    );
    say_words(&mut app, &script, "read answers aloud for spoken questions");
    assert_eq!(
        app.voice.prefs.read_aloud,
        ReadAloud::Spoken,
        "{}",
        app.status
    );
    say_words(&mut app, &script, "set the voice language to French");
    assert_eq!(
        app.voice.prefs.language.as_deref(),
        Some("fr"),
        "{}",
        app.status
    );
    // Spoken commands off, what is said goes to Ask — but for turning them
    // back on, which nothing else said could.
    say_words(&mut app, &script, "turn off spoken commands");
    assert!(!app.voice.prefs.run_commands, "{}", app.status);
    app.voice.prefs.send_questions = false;
    say_words(&mut app, &script, "dark mode");
    assert_eq!(
        app.voice.heard.as_ref().map(|h| &h.outcome),
        Some(&HeardOutcome::Asked)
    );
    app.proj.ask_input.clear();
    say_words(&mut app, &script, "turn spoken commands on");
    assert!(app.voice.prefs.run_commands, "{}", app.status);
}

/// A dialog takes what is said as it takes the pointer: a note's words
/// typed into it, its Save said by name; Settings put away by "close".
#[test]
fn a_dialog_is_filled_and_answered_by_voice() {
    let (mut app, script, _) = voice_app("voice-dialog", "");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let seed = vec![bookmarks::Bookmark {
        rel: "src/lib.rs".into(),
        line: 3,
        preview: "fn".into(),
        note: None,
    }];
    bookmarks::save(&root, &seed).unwrap();
    app.proj.bookmarks = bookmarks::load(&root);
    let _ = app.update(Message::Reading(ReadingMsg::BookmarkNoteEdit(
        "src/lib.rs".into(),
        3,
    )));
    // What only opens as a yes does ("run …") is no answer to the
    // question on top: it is the note's words.
    say_words(&mut app, &script, "run the tests");
    assert!(app.proj.note_edit.is_some(), "{}", app.status);
    let _ = app.update(Message::Reading(ReadingMsg::BookmarkNoteInput(
        String::new(),
    )));
    say_words(&mut app, &script, "Why this line matters.");
    say_words(&mut app, &script, "and what it returns");
    say_words(&mut app, &script, "Save");
    assert!(app.proj.note_edit.is_none(), "{}", app.status);
    assert_eq!(
        app.proj.bookmarks.first().and_then(|b| b.note.as_deref()),
        Some("Why this line matters and what it returns")
    );

    let _ = app.update(Message::Settings(SettingsMsg::Open));
    assert!(app.settings.open);
    say_words(&mut app, &script, "close");
    assert!(!app.settings.open, "{}", app.status);
    // Put away as Esc does, by a word no button of it carries.
    let _ = app.update(Message::Settings(SettingsMsg::Open));
    say_words(&mut app, &script, "never mind");
    assert!(!app.settings.open, "{}", app.status);
}

/// A row of the list in view is picked by its place in it, as it is
/// counted down the list.
#[test]
fn a_rows_place_in_the_list_picks_it() {
    let (mut app, script, _) = voice_app("voice-row", "");
    let _ = app.update(Message::Window(WindowMsg::SidebarTabPicked(
        SidebarTab::Files,
    )));
    let _ = app.update(Message::Window(WindowMsg::FileFilterChanged("s".into())));
    let listed: Vec<String> = crate::ui::filtered_files(app.proj.project.as_ref().unwrap(), "s")
        .map(|f| f.rel.clone())
        .collect();
    assert!(listed.len() >= 2, "{listed:?}");
    say_words(&mut app, &script, "open the second one");
    assert_eq!(
        app.active_viewer().map(|v| v.rel.clone()),
        Some(listed[1].clone()),
        "{}",
        app.status
    );
    // Opened, a file may ask about its language's server: a question on
    // top takes what is said, and is answered so.
    if crate::ui::active_overlay(&app).is_some() {
        say_words(&mut app, &script, "not now");
        assert!(crate::ui::active_overlay(&app).is_none(), "{}", app.status);
    }
    say_words(&mut app, &script, "the first result");
    assert_eq!(
        app.active_viewer().map(|v| v.rel.clone()),
        Some(listed[0].clone()),
        "{}",
        app.status
    );
}

/// What undoes something waits for a yes when it is said — a word may
/// have been misheard — and a no leaves it.
#[test]
fn what_undoes_waits_for_a_yes() {
    let (mut app, script, _) = voice_app("voice-undoes", "");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let seed = vec![bookmarks::Bookmark {
        rel: "src/lib.rs".into(),
        line: 3,
        preview: "fn".into(),
        note: None,
    }];
    bookmarks::save(&root, &seed).unwrap();
    app.proj.bookmarks = bookmarks::load(&root);
    let _ = app.update(Message::Window(WindowMsg::SidebarTabPicked(
        SidebarTab::Marks,
    )));
    say_words(&mut app, &script, "remove src/lib.rs 3");
    assert!(
        matches!(
            &app.voice.confirm,
            Some(crate::app::voice::Waiting::Press { .. })
        ),
        "{}",
        app.status
    );
    assert_eq!(app.proj.bookmarks.len(), 1, "not before a yes");
    say_words(&mut app, &script, "no");
    assert_eq!(app.proj.bookmarks.len(), 1);
    say_words(&mut app, &script, "remove src/lib.rs 3");
    say_words(&mut app, &script, "yes");
    assert!(app.proj.bookmarks.is_empty(), "{}", app.status);
}

/// A target of the window, as `ui::heard` offers one.
fn on_screen(
    title: &str,
    message: Message,
    layer: crate::ui::heard::Layer,
    answer: Option<crate::ui::heard::Answer>,
) -> crate::ui::heard::Target {
    crate::ui::heard::Target {
        title: title.into(),
        shown: title.into(),
        message,
        layer,
        row: None,
        answer,
        typing: None,
    }
}

/// "Never mind" and "cancel" put away what is on top: they answer a
/// question only when it is the one on top — a dialog — never a card in
/// Ask the reader was not looking at, which "no" still answers.
#[test]
fn a_word_that_closes_answers_only_the_question_on_top() {
    use crate::ui::heard::{Answer, Layer};
    let mut app = blank_app();
    let deny = |layer| {
        on_screen(
            "Deny",
            Message::Window(WindowMsg::OpenShortcuts),
            layer,
            Some(Answer::No),
        )
    };
    let card = [deny(Layer::Ask)];
    assert!(app.heard_on_screen("never mind", &card, true).is_none());
    assert!(!app.status.contains("Deny"), "{}", app.status);
    assert!(app.heard_on_screen("no", &card, true).is_some());
    assert!(app.status.contains("Deny"), "{}", app.status);
    app.status.clear();
    let dialog = [deny(Layer::Overlay)];
    assert!(app.heard_on_screen("never mind", &dialog, true).is_some());
    assert!(app.status.contains("Deny"), "{}", app.status);
}

/// Said to a dialog with a field and a list — the command bar — a row's
/// place opens that row; it is not typed into the field.
#[test]
fn a_rows_place_said_to_the_command_bar_opens_it() {
    use crate::ui::heard::Layer;
    let mut app = blank_app();
    let mut targets = vec![crate::ui::heard::Target {
        typing: Some((String::new(), |q| {
            Message::Nav(NavMsg::FinderQueryChanged(q))
        })),
        ..on_screen("", Message::Noop, Layer::Overlay, None)
    }];
    for i in 1..=3 {
        targets.push(crate::ui::heard::Target {
            row: Some((1, i)),
            ..on_screen(
                &format!("file{i}.rs"),
                Message::Window(WindowMsg::OpenShortcuts),
                Layer::Overlay,
                None,
            )
        });
    }
    assert!(
        app.heard_on_screen("the second one", &targets, true)
            .is_some()
    );
    assert!(app.status.contains("file2.rs"), "{}", app.status);
    assert!(!app.status.contains("Typed"), "{}", app.status);
}

/// A button that costs something, pressed by a word that may have been
/// misheard, waits for a yes as the command said does — Explain, here —
/// but the answer to a question asked is pressed: it was asked already.
#[test]
fn a_costly_button_said_waits_for_a_yes() {
    use crate::ui::heard::{Answer, Layer};
    let mut app = blank_app();
    let explain = Message::Editor(EditorMsg::RunAction(crate::keymap::Action::ExplainAll));
    let targets = [on_screen(
        "Explain All",
        explain.clone(),
        Layer::Window,
        None,
    )];
    assert!(
        app.heard_on_screen("explain all", &targets, false)
            .is_some()
    );
    assert!(
        matches!(
            &app.voice.confirm,
            Some(crate::app::voice::Waiting::Press { .. })
        ),
        "{}",
        app.status
    );
    app.voice.confirm = None;
    let asked = [on_screen(
        "Explain All",
        explain,
        Layer::Overlay,
        Some(Answer::Yes),
    )];
    assert!(app.heard_on_screen("explain all", &asked, true).is_some());
    assert!(app.voice.confirm.is_none(), "an answer is asked again");
    // What only shows, or stops spending, is pressed at once: a variable
    // opened, an explain pass stopped. Stopping the program asks, as
    // starting it does — a misheard word would end the session.
    for (title, message, asks) in [
        ("total", Message::Debug(DebugMsg::ToggleVar(7)), false),
        (
            "Stop explaining",
            Message::Explain(ExplainMsg::Cancel),
            false,
        ),
        ("Stop", Message::Debug(DebugMsg::Stop), true),
    ] {
        let mut app = blank_app();
        let targets = [on_screen(title, message, Layer::Window, None)];
        assert!(
            app.heard_on_screen(&title.to_lowercase(), &targets, false)
                .is_some()
        );
        assert_eq!(app.voice.confirm.is_some(), asks, "{title}");
    }
}

/// The model's provider, picked with Settings shut — said, or run from
/// the command bar — is kept, as picked on its page and closed, when it
/// has a key of its own: here, its variable's. Without one, its page opens
/// to ask for it and nothing is saved meanwhile. A key kept for another
/// provider is never carried to it — it would be sent there.
#[test]
fn a_provider_picked_with_settings_shut_is_kept_with_its_own_key() {
    use crate::llm::{Config, Provider};
    let data = test_dir("voice-provider-shut");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let _keys = EnvVars::new()
        .remove("ANTHROPIC_API_KEY")
        .remove("OPENAI_API_KEY")
        .remove("DEEPSEEK_API_KEY");
    Config::from_parts(
        Provider::Anthropic,
        "sk-ant-SECRET".into(),
        String::new(),
        String::new(),
    )
    .save()
    .unwrap();
    let mut app = blank_app();
    // Typing in Ask, say, as it was said.
    app.code_focused = false;
    assert!(!app.settings.open);
    // OpenAI, with no key of its own: its page asks for one.
    let _ = app.update(Message::Settings(SettingsMsg::ProviderPicked(
        Provider::OpenAI,
    )));
    assert!(app.settings.open, "{}", app.status);
    assert_eq!(app.settings.page, SettingsPage::LanguageModel);
    assert_eq!(app.settings.key, "", "Anthropic's key, offered to OpenAI");
    assert!(app.status.contains("OpenAI"), "{}", app.status);
    assert_eq!(Config::current_or_default().provider, Provider::Anthropic);
    // Back to the provider it opened on: its own key again.
    let _ = app.update(Message::Settings(SettingsMsg::ProviderPicked(
        Provider::Anthropic,
    )));
    assert_eq!(app.settings.key, "sk-ant-SECRET");
    let _ = app.update(Message::Settings(SettingsMsg::Close));
    assert_eq!(Config::stored_key(), "sk-ant-SECRET");
    // OpenAI's own key in its variable: kept at once, Settings shut, the
    // keyboard where it was — and Anthropic's key not written for it.
    app.code_focused = false;
    let _openai = EnvVars::new().set("OPENAI_API_KEY", "sk-openai-ENV");
    let _ = app.update(Message::Settings(SettingsMsg::ProviderPicked(
        Provider::OpenAI,
    )));
    assert!(!app.settings.open, "{}", app.status);
    let kept = Config::current_or_default();
    assert_eq!(kept.provider, Provider::OpenAI);
    assert_eq!(kept.api_key, "sk-openai-ENV");
    assert_eq!(Config::stored_key(), "");
    assert!(!app.code_focused, "the keyboard went to the code");
}

/// Picked with Settings shut, a page's own pick with nothing to keep
/// asked of it — embeddings made here, their model not downloaded —
/// opens the page on what it needs.
#[test]
fn embeddings_picked_with_settings_shut_ask_for_what_they_need() {
    let data = test_dir("voice-embed-shut");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let _keys = EnvVars::new().remove("OPENAI_API_KEY");
    let mut app = blank_app();
    let here = !clew_core::embed::Config::stored_here();
    let _ = app.update(Message::Settings(SettingsMsg::EmbedHere(here)));
    if here && clew_core::embed::local::installed() {
        return;
    }
    assert!(app.settings.open, "{}", app.status);
    assert_eq!(app.settings.page, SettingsPage::Embeddings);
    assert_eq!(
        clew_core::embed::Config::stored_here(),
        !here,
        "nothing saved"
    );
}

/// What is on screen is the reader's to press by voice, never the agent's:
/// a question's answers are no palette command.
#[test]
fn what_is_on_screen_is_not_the_agents() {
    let mut app = blank_app();
    let _ = app.update(Message::Settings(SettingsMsg::OpenAt(SettingsPage::Voice)));
    let targets = crate::ui::heard::gather(&app);
    assert!(
        targets.iter().any(|t| t.title.starts_with("Download")),
        "{:?}",
        targets.iter().map(|t| &t.title).collect::<Vec<_>>()
    );
    // A switch by its name and state, and a picker's every choice.
    for title in [
        "Run spoken commands: Off",
        "Run spoken commands: On",
        "1.25×",
    ] {
        assert!(targets.iter().any(|t| t.title == title), "{title}");
    }
    // Each model's Download named with it, as every model has one.
    assert!(
        targets
            .iter()
            .any(|t| t.title.starts_with("Download") && t.title.ends_with(": Large Turbo")),
        "{:?}",
        targets.iter().map(|t| &t.title).collect::<Vec<_>>()
    );
    let palette = app.palette_commands();
    assert!(!palette.iter().any(|c| c.title.starts_with("Download")));
}
