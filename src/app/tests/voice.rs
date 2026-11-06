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

fn fake_mic(_max_seconds: u32) -> Result<Box<dyn Listening>, String> {
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
}

impl Transcriber for Hears {
    fn transcribe(
        &self,
        _model: &Path,
        audio: &[f32],
        options: &Options,
        cancel: &AtomicBool,
    ) -> Result<String, String> {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        self.hints.lock().unwrap().push(options.hint.clone());
        *self.heard_audio.lock().unwrap() = audio.len();
        Ok(self.words.clone())
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

/// Listen, hear a pause after speech, and feed back what was heard.
fn say(app: &mut App, script: &Arc<Mutex<MicScript>>) {
    app.voice.reset_for_test();
    script.lock().unwrap().silence = None;
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(app.voice.is_listening(), "{}", app.status);
    script.lock().unwrap().silence = Some(END_SILENCE + 0.1);
    let task = app.update(Message::Voice(VoiceMsg::Tick));
    assert!(matches!(app.voice.phase, VoicePhase::Transcribing { .. }));
    assert!(script.lock().unwrap().stopped, "the microphone is closed");
    for message in run_task(task) {
        let _ = app.update(message);
    }
}

#[test]
fn a_command_heard_runs_as_the_palette_runs_it() {
    let (mut app, script, hears) = voice_app("voice-command", "Open the call graph.");
    say(&mut app, &script);
    assert_eq!(app.voice.phase, VoicePhase::Idle);
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

/// A command that costs something waits for a yes; Esc is a no.
#[test]
fn a_costly_command_asks_first() {
    let (mut app, script, _) = voice_app("voice-costly", "explain all");
    say(&mut app, &script);
    assert_eq!(app.voice.confirm.as_deref(), Some("Explain All"));
    assert!(!app.settings.open, "nothing ran yet");
    assert!(app.voice.is_busy());
    // Esc: no.
    let _ = app.update(key_press(
        keyboard::Key::Named(keyboard::key::Named::Escape),
        keyboard::Modifiers::empty(),
    ));
    assert_eq!(app.voice.confirm, None);
    assert!(!app.settings.open);
    // Again, and yes: it runs (with no key to pay for it, Settings asks
    // for one).
    say(&mut app, &script);
    assert_eq!(app.voice.confirm.as_deref(), Some("Explain All"));
    let _ = app.update(Message::Voice(VoiceMsg::Confirm));
    assert_eq!(app.voice.confirm, None);
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
    // so the question waits in the input, said why.
    let (mut app, script, _) = voice_app("voice-question-send", "where is the origin defined");
    say(&mut app, &script);
    assert_eq!(app.ask_view, AskView::Compact);
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

/// Keep listening: once Clew has answered, the microphone opens again for
/// what is said next — not after a Stop, not with the switch off, and not
/// with the conversation put away.
#[test]
fn keep_listening_opens_the_microphone_after_an_answer() {
    use clew_protocol::StreamOutcome;
    let (mut app, _script, _) = voice_app("voice-keep-listening", "");
    app.ask_view = AskView::Compact;
    let turn = |app: &mut App, stream: u64| {
        app.proj.ask_turns.push(crate::app::model::AskTurn {
            stream,
            question: "where is it?".into(),
            streaming: true,
            ..Default::default()
        });
    };
    // Off: the answer ends, the microphone stays shut.
    turn(&mut app, 1);
    let _ = app.on_ask_stream_ended(1, StreamOutcome::Done);
    assert_eq!(app.voice.phase, VoicePhase::Idle);
    // On: it opens.
    let _ = app.update(Message::Voice(crate::VoiceMsg::SetKeepListening(true)));
    turn(&mut app, 2);
    let _ = app.on_ask_stream_ended(2, StreamOutcome::Done);
    assert!(
        matches!(app.voice.phase, VoicePhase::Listening { .. }),
        "{:?}",
        app.voice.phase
    );
    let _ = app.update(Message::Voice(crate::VoiceMsg::Cancel));
    // A Stop is the reader's: the microphone stays shut.
    turn(&mut app, 3);
    let _ = app.on_ask_stream_ended(3, StreamOutcome::Stopped);
    assert_eq!(app.voice.phase, VoicePhase::Idle);
    // Put away, the conversation hears nothing.
    app.ask_view = AskView::Closed;
    turn(&mut app, 4);
    let _ = app.on_ask_stream_ended(4, StreamOutcome::Done);
    assert_eq!(app.voice.phase, VoicePhase::Idle);
}

/// Nothing heard, nothing done; a recognizer that fails says why.
#[test]
fn silence_and_failures_do_nothing_but_say_so() {
    let (mut app, script, _) = voice_app("voice-empty", "");
    say(&mut app, &script);
    assert_eq!(app.voice.phase, VoicePhase::Idle);
    assert_eq!(
        app.voice.heard.as_ref().map(|h| &h.outcome),
        Some(&HeardOutcome::Nothing)
    );
    assert!(app.proj.ask_input.is_empty());
    assert!(!app.ask_view.is_open(), "nothing heard opens no card");
    // Nothing said for long enough: it stops listening on its own.
    app.voice.reset_for_test();
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(app.voice.is_listening(), "a repeat of the key, ignored");
    script.lock().unwrap().silence = None;
    if let VoicePhase::Listening { since, .. } = &mut app.voice.phase {
        *since -= std::time::Duration::from_secs_f32(NO_SPEECH_TIMEOUT + 1.0);
    }
    let _ = app.update(Message::Voice(VoiceMsg::Tick));
    assert_eq!(app.voice.phase, VoicePhase::Idle);
    assert!(app.status.contains("Nothing was heard"), "{}", app.status);
    // The microphone going away mid-way.
    app.voice.reset_for_test();
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    script.lock().unwrap().error = Some("the microphone is not available".into());
    let _ = app.update(Message::Voice(VoiceMsg::Tick));
    assert_eq!(app.voice.phase, VoicePhase::Idle);
    assert!(app.status.contains("not available"), "{}", app.status);
}

/// Esc stops listening and keeps nothing; a transcription stopped so
/// comes to nothing when it lands.
#[test]
fn esc_cancels_listening_and_transcribing() {
    let (mut app, script, _) = voice_app("voice-esc", "open the call graph");
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    assert!(app.voice.is_listening());
    let _ = app.update(key_press(
        keyboard::Key::Named(keyboard::key::Named::Escape),
        keyboard::Modifiers::empty(),
    ));
    assert_eq!(app.voice.phase, VoicePhase::Idle);
    assert!(script.lock().unwrap().stopped);
    // Stopped by hand, then cancelled while transcribing.
    app.voice.reset_for_test();
    let _ = app.update(Message::Voice(VoiceMsg::Toggle));
    let task = app.update(Message::Voice(VoiceMsg::Stop));
    assert!(matches!(app.voice.phase, VoicePhase::Transcribing { .. }));
    let _ = app.update(Message::Voice(VoiceMsg::Cancel));
    for message in run_task(task) {
        let _ = app.update(message);
    }
    assert_eq!(app.proj.overlay, None, "the words came too late");
    assert_eq!(app.voice.phase, VoicePhase::Idle);
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
        error.contains("not ggml-base.en.bin as published"),
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
