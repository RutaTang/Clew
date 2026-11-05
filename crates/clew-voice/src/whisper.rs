//! Speech to text on this machine: whisper.cpp, through whisper-rs, run on
//! a model file from [`crate::models`]. Nothing leaves the machine.
//!
//! The GPU is used where whisper.cpp was built for one (Metal on macOS);
//! elsewhere it runs on every core the machine offers, which the small
//! models need no more than a moment of for a spoken command.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// How a clip is transcribed.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The language spoken, as whisper's code (`en`, `de`); `None` to let a
    /// multilingual model tell. An English-only model always hears English.
    pub language: Option<String>,
    /// Words the speaker is likely to use — the window's command names,
    /// the project's — given to whisper as the text that came before, so it
    /// spells them as they are spelled there. Cut to [`MAX_HINT`].
    pub hint: String,
    /// Threads to run on; 0 for one per core, up to eight.
    pub threads: usize,
}

/// Longest hint passed on: whisper reads at most half its context (224
/// tokens) of prompt and drops the start of a longer one.
pub const MAX_HINT: usize = 600;

/// A loaded model.
pub struct Whisper {
    ctx: WhisperContext,
    path: PathBuf,
}

static QUIET: Once = Once::new();

impl Whisper {
    /// Load the model at `path`.
    pub fn load(path: &Path) -> Result<Whisper, String> {
        // whisper.cpp and ggml log every step of a load and a run to
        // stderr; these hooks take that over (and, with no logging backend
        // in the build, keep it quiet).
        QUIET.call_once(whisper_rs::install_logging_hooks);
        if !path.is_file() {
            return Err(format!("no speech model at {}", path.display()));
        }
        let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .map_err(|e| format!("could not load the speech model {}: {e}", path.display()))?;
        Ok(Whisper {
            ctx,
            path: path.to_path_buf(),
        })
    }

    /// The file it was loaded from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether it understands more than English.
    pub fn multilingual(&self) -> bool {
        self.ctx.is_multilingual()
    }

    /// The text spoken in `audio` (16 kHz, one channel), cleaned of
    /// whisper's sound annotations ([`clean`]). Stops early, with an error,
    /// once `cancel` is set.
    pub fn transcribe(
        &self,
        audio: &[f32],
        options: &Options,
        cancel: &AtomicBool,
    ) -> Result<String, String> {
        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| format!("could not start the speech model: {e}"))?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        let language = match (&options.language, self.multilingual()) {
            (_, false) => "en".to_string(),
            (Some(lang), true) => lang.clone(),
            (None, true) => "auto".to_string(),
        };
        params.set_language(Some(&language));
        params.set_translate(false);
        params.set_n_threads(threads(options.threads));
        // One utterance at a time: nothing carried over from the last, no
        // timestamps, and no tokens for sounds that are not speech.
        params.set_no_context(true);
        params.set_no_timestamps(true);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_temperature(0.0);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        let hint = hint(&options.hint);
        if !hint.is_empty() {
            params.set_initial_prompt(hint);
        }
        // SAFETY: the callback only reads the flag through the pointer, and
        // `cancel` outlives the `full` call that is the only caller of it
        // (`params` is moved into it and dropped by it).
        unsafe {
            params.set_abort_callback(Some(abort_requested));
            params.set_abort_callback_user_data(cancel as *const AtomicBool as *mut c_void);
        }
        state.full(params, audio).map_err(|e| {
            if cancel.load(Ordering::Relaxed) {
                "cancelled".to_string()
            } else {
                format!("the speech model failed: {e}")
            }
        })?;
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let text: String = state
            .as_iter()
            .filter_map(|segment| segment.to_str_lossy().ok().map(|s| s.into_owned()))
            .collect();
        Ok(clean(&text))
    }
}

/// whisper.cpp's abort callback: whether the flag `user_data` points to
/// is set.
unsafe extern "C" fn abort_requested(user_data: *mut c_void) -> bool {
    // SAFETY: set to a live `&AtomicBool` for the length of the run.
    let flag = unsafe { &*(user_data as *const AtomicBool) };
    flag.load(Ordering::Relaxed)
}

fn threads(wanted: usize) -> i32 {
    let n = if wanted == 0 {
        std::thread::available_parallelism().map_or(4, |n| n.get().min(8))
    } else {
        wanted
    };
    n.clamp(1, 64) as i32
}

/// The end of `hint` that fits in [`MAX_HINT`] bytes, cut at a word. The
/// end, because whisper keeps the end of a prompt that is too long.
pub fn hint(hint: &str) -> &str {
    let hint = hint.trim();
    if hint.len() <= MAX_HINT {
        return hint;
    }
    let mut start = hint.len() - MAX_HINT;
    while !hint.is_char_boundary(start) {
        start += 1;
    }
    let tail = &hint[start..];
    match tail.find(char::is_whitespace) {
        Some(i) => tail[i..].trim_start(),
        None => tail,
    }
}

/// What whisper wrote, without what it adds of its own: sound annotations
/// (`[BLANK_AUDIO]`, `(music)`, `*sighs*`), the stock line it puts on a
/// clip it heard nothing in, and spacing.
pub fn clean(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth: Option<char> = None;
    for c in raw.chars() {
        match (depth, c) {
            (None, '[') => depth = Some(']'),
            (None, '(') => depth = Some(')'),
            (None, '*') => depth = Some('*'),
            (Some(close), c) if c == close => {
                depth = None;
                out.push(' ');
            }
            (Some(_), _) => {}
            (None, c) => out.push(c),
        }
    }
    let words: Vec<&str> = out.split_whitespace().collect();
    let text = words.join(" ");
    let text = text.trim_start_matches(['-', ' ']).trim();
    // What whisper says of a clip of noise. Never a request.
    let bare: String = text
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    if matches!(
        bare.trim(),
        "" | "you" | "thanks for watching" | "thank you for watching" | "bye"
    ) {
        return String::new();
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whispers_annotations_are_dropped() {
        assert_eq!(clean(" [BLANK_AUDIO]"), "");
        assert_eq!(clean(" Open the call graph."), "Open the call graph.");
        assert_eq!(
            clean(" (keyboard clicking) Show  the minimap [Music]"),
            "Show the minimap"
        );
        assert_eq!(clean("*sighs* go to definition"), "go to definition");
        assert_eq!(clean(" - Zoom in"), "Zoom in");
        assert_eq!(clean(" you"), "");
        assert_eq!(clean(" Thanks for watching!"), "");
        assert_eq!(clean("Thank you."), "Thank you.");
        assert_eq!(clean("  "), "");
    }

    #[test]
    fn a_long_hint_keeps_its_end_at_a_word() {
        assert_eq!(hint("  Call Graph, Overview  "), "Call Graph, Overview");
        let long = "word ".repeat(200) + "Last";
        let h = hint(&long);
        assert!(h.len() <= MAX_HINT, "{}", h.len());
        assert!(h.ends_with("Last"));
        assert!(h.starts_with("word"), "{h}");
        // Never cut inside a character.
        let wide = "é".repeat(400);
        assert!(hint(&wide).len() <= MAX_HINT);
    }

    #[test]
    fn threads_default_to_the_cores() {
        assert!((1..=8).contains(&threads(0)));
        assert_eq!(threads(3), 3);
    }

    #[test]
    fn a_missing_or_broken_model_is_an_error_not_a_crash() {
        let missing = std::env::temp_dir().join("clew-voice-no-such-model.bin");
        let err = Whisper::load(&missing).err().unwrap();
        assert!(err.contains("no speech model"), "{err}");
        let junk = std::env::temp_dir().join(format!("clew-voice-junk-{}.bin", std::process::id()));
        std::fs::write(&junk, b"not a ggml model at all").unwrap();
        let err = Whisper::load(&junk).err().unwrap();
        assert!(err.contains("could not load"), "{err}");
        let _ = std::fs::remove_file(&junk);
    }

    /// Spoken commands, through everything voice does with them — the
    /// clip resampled from its own rate, cut to its speech, transcribed with
    /// the window's command names as the hint, and read as a command: each
    /// must come out as the command it names. Opt in with
    /// `CLEW_TEST_WHISPER_MODEL` (a real model) and
    /// `CLEW_TEST_SPOKEN_COMMANDS`, a folder of WAV clips with a
    /// `commands.txt` of `clip.wav|Title` lines.
    #[test]
    fn spoken_commands_come_out_as_the_commands_they_name() {
        let (Ok(model), Ok(dir)) = (
            std::env::var("CLEW_TEST_WHISPER_MODEL"),
            std::env::var("CLEW_TEST_SPOKEN_COMMANDS"),
        ) else {
            eprintln!("skipped: set CLEW_TEST_WHISPER_MODEL and CLEW_TEST_SPOKEN_COMMANDS");
            return;
        };
        const TITLES: &[&str] = &[
            "Call Graph",
            "Import Graph",
            "Go to File…",
            "Go to Symbol…",
            "Go to Definition",
            "Find References",
            "Zoom In",
            "Zoom Out",
            "Overview",
            "Stats",
            "Settings",
            "Sidebar: Files",
            "Sidebar: Marks",
            "View: Hide Minimap",
            "View: Show Context Panel",
            "Theme: Light",
            "Theme: Dark",
            "Explain All",
        ];
        let whisper = Whisper::load(Path::new(&model)).unwrap();
        let options = Options {
            hint: TITLES
                .iter()
                .map(|t| t.trim_end_matches('…'))
                .collect::<Vec<_>>()
                .join(", "),
            ..Options::default()
        };
        let dir = Path::new(&dir);
        let index = std::fs::read_to_string(dir.join("commands.txt")).unwrap();
        let mut wrong = Vec::new();
        for line in index.lines().filter(|l| !l.trim().is_empty()) {
            let (clip, title) = line.split_once('|').unwrap();
            let (samples, rate) =
                crate::wav::read(&std::fs::read(dir.join(clip)).unwrap()).unwrap();
            let audio = crate::audio::prepare(&samples, rate).expect("speech in the clip");
            let started = std::time::Instant::now();
            let text = whisper
                .transcribe(&audio, &options, &AtomicBool::new(false))
                .unwrap();
            let heard = crate::intent::command(&text, TITLES).map(|i| TITLES[i]);
            eprintln!(
                "{clip} at {rate} Hz: {text:?} → {heard:?} in {:.1?}",
                started.elapsed()
            );
            if heard != Some(title) {
                wrong.push(format!("{clip}: {text:?} → {heard:?}, not {title}"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// A real model, transcribing real speech: opt in with
    /// `CLEW_TEST_WHISPER_MODEL` (a ggml model, such as whisper.cpp's
    /// `ggml-tiny.en.bin`) and `CLEW_TEST_WHISPER_WAV` (a PCM WAV, such as
    /// whisper.cpp's `samples/jfk.wav`), and `CLEW_TEST_WHISPER_EXPECT`,
    /// words the transcript must contain. Without the last it only runs the
    /// whole way through — which whisper.cpp's own weightless test models
    /// (`models/for-tests-ggml-*.bin`) can do, if not hear anything.
    #[test]
    fn transcribes_real_speech_with_a_real_model() {
        let (Ok(model), Ok(wav)) = (
            std::env::var("CLEW_TEST_WHISPER_MODEL"),
            std::env::var("CLEW_TEST_WHISPER_WAV"),
        ) else {
            eprintln!("skipped: set CLEW_TEST_WHISPER_MODEL and CLEW_TEST_WHISPER_WAV");
            return;
        };
        let (samples, rate) = crate::wav::read(&std::fs::read(wav).unwrap()).unwrap();
        let audio = crate::audio::prepare(&samples, rate).expect("speech in the clip");
        let whisper = Whisper::load(Path::new(&model)).unwrap();
        let started = std::time::Instant::now();
        let text = whisper
            .transcribe(&audio, &Options::default(), &AtomicBool::new(false))
            .unwrap();
        eprintln!("{:?} in {:?}", text, started.elapsed());
        if let Ok(expect) = std::env::var("CLEW_TEST_WHISPER_EXPECT") {
            assert!(!text.is_empty());
            let lower = text.to_lowercase();
            for word in expect.split_whitespace() {
                assert!(lower.contains(&word.to_lowercase()), "{word} in {text}");
            }
        }
        // Cancelled before it starts, it stops at once.
        let err = whisper
            .transcribe(&audio, &Options::default(), &AtomicBool::new(true))
            .unwrap_err();
        assert_eq!(err, "cancelled");
    }
}
