//! What Clew says, made by `clew-speak` — the program beside Clew's own
//! that runs a voice model ([`crate::speech`]) — through its requests and
//! answers on stdin and stdout, one JSON line each.
//!
//! [`Speakers`] keeps one running, for the model last spoken with, so the
//! model is loaded once for a whole answer, and starts it again if it died.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Deserialize;

use crate::speech::VoiceModel;

/// How long loading a model may take.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long one sentence — or, of a voice that streams, the next part of
/// it — may take to be made.
const SAY_TIMEOUT: Duration = Duration::from_secs(120);

/// What `clew-speak` answers.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Answer {
    Ready {
        rate: u32,
        voices: u32,
    },
    /// A sentence's audio — or, of a voice that streams, a part of it, with
    /// `more` to come.
    Audio {
        id: u64,
        pcm: String,
        #[serde(default)]
        more: bool,
    },
    Failed {
        id: u64,
        error: String,
    },
    Error(String),
}

/// Why something could not be said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SayError {
    /// This sentence: the voice is fine, and what comes next can be said.
    Sentence(String),
    /// The voice: it could not start, or it ended, or it stopped answering —
    /// nothing more will be said until whatever is wrong is put right.
    Voice(String),
}

impl std::fmt::Display for SayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SayError::Sentence(e) | SayError::Voice(e) => f.write_str(e),
        }
    }
}

/// One `clew-speak`, with a model loaded.
pub struct Speaker {
    child: Child,
    stdin: ChildStdin,
    answers: mpsc::Receiver<Result<Answer, String>>,
    rate: u32,
    voices: u32,
    next: u64,
}

impl Speaker {
    /// Start `program` with `model`, whose files are in `dir`, and wait for
    /// it to have loaded it.
    pub fn start(program: &Path, model: &VoiceModel, dir: &Path) -> Result<Speaker, String> {
        let mut command = Command::new(program);
        command
            .arg(model.family)
            .arg(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window of its own.
            command.creation_flags(0x0800_0000);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("could not start the voice: {e}"))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err("could not connect to the voice".into());
        };
        let (tx, answers) = mpsc::channel();
        std::thread::Builder::new()
            .name("clew-speak-answers".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let answer = match line {
                        Ok(line) => serde_json::from_str::<Answer>(&line)
                            .map_err(|e| format!("the voice said something unreadable: {e}")),
                        Err(e) => Err(format!("the voice's reply could not be read: {e}")),
                    };
                    let stop = answer.is_err();
                    if tx.send(answer).is_err() || stop {
                        break;
                    }
                }
            })
            .map_err(|e| format!("could not start the voice: {e}"))?;
        let mut speaker = Speaker {
            child,
            stdin,
            answers,
            rate: 0,
            voices: 0,
            next: 0,
        };
        match speaker.answers.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(Answer::Ready { rate, voices })) => {
                speaker.rate = rate;
                speaker.voices = voices;
                Ok(speaker)
            }
            Ok(Ok(Answer::Error(e))) | Ok(Err(e)) => Err(e),
            Ok(Ok(_)) => Err("the voice answered before it was ready".into()),
            Err(mpsc::RecvTimeoutError::Timeout) => Err("the voice took too long to load".into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("the voice stopped while starting — its model may be damaged".into())
            }
        }
    }

    /// The rate it speaks at, in Hz.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// How many voices its model has.
    pub fn voices(&self) -> u32 {
        self.voices
    }

    /// `text` said in `lang` by voice `voice` at `speed`, given to `part` as
    /// it comes — whole, or a part at a time from a voice that streams: one
    /// channel, at [`Speaker::rate`].
    pub fn say(
        &mut self,
        text: &str,
        lang: &str,
        voice: i32,
        speed: f32,
        part: &mut dyn FnMut(Vec<f32>),
    ) -> Result<(), SayError> {
        self.next += 1;
        let id = self.next;
        let request = serde_json::json!({
            "say": {"id": id, "text": text, "lang": lang, "voice": voice, "speed": speed}
        });
        writeln!(self.stdin, "{request}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| SayError::Voice(format!("the voice stopped accepting text: {e}")))?;
        loop {
            match self.answers.recv_timeout(SAY_TIMEOUT) {
                Ok(Ok(Answer::Audio { id: got, pcm, more })) if got == id => {
                    let samples = decode(&pcm).map_err(SayError::Voice)?;
                    if !samples.is_empty() {
                        part(samples);
                    }
                    if !more {
                        return Ok(());
                    }
                }
                Ok(Ok(Answer::Failed { id: got, error })) if got == id => {
                    return Err(SayError::Sentence(error));
                }
                // An answer to an earlier request, given up on.
                Ok(Ok(Answer::Audio { .. } | Answer::Failed { .. })) => {}
                Ok(Ok(Answer::Ready { .. })) => {}
                Ok(Ok(Answer::Error(e))) | Ok(Err(e)) => return Err(SayError::Voice(e)),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Still making it, maybe: it is not asked again, and what
                    // it is busy with is not waited for — a fresh one starts.
                    let _ = self.child.kill();
                    return Err(SayError::Voice("the voice took too long to speak".into()));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(SayError::Voice("the voice ended unexpectedly".into()));
                }
            }
        }
    }

    /// Whether it has ended.
    fn ended(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 16-bit little-endian PCM, in base64, as samples in [-1, 1].
fn decode(pcm: &str) -> Result<Vec<f32>, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(pcm)
        .map_err(|e| format!("the voice's audio is unreadable: {e}"))?;
    Ok(bytes
        .chunks_exact(2)
        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
        .collect())
}

/// The `clew-speak` running, for the model last spoken with.
pub struct Speakers {
    program: PathBuf,
    running: Mutex<Option<Running>>,
}

/// The `clew-speak` running: its model, and when it last said something.
struct Running {
    model: &'static str,
    speaker: Speaker,
    used: Instant,
}

impl Speakers {
    /// Speakers run as `program`.
    pub fn new(program: PathBuf) -> Self {
        Self {
            program,
            running: Mutex::new(None),
        }
    }

    /// `text` said by `model` (its files in `dir`), given to `part` as it
    /// comes ([`Speaker::say`]) with its rate. The model is loaded once,
    /// kept for what is said next with it, and started again if it ended.
    #[allow(clippy::too_many_arguments)]
    pub fn say(
        &self,
        model: &'static VoiceModel,
        dir: &Path,
        text: &str,
        lang: &str,
        voice: i32,
        speed: f32,
        part: &mut dyn FnMut(Vec<f32>, u32),
    ) -> Result<(), SayError> {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let reuse = running
            .as_mut()
            .is_some_and(|r| r.model == model.id && !r.speaker.ended());
        if !reuse {
            // Removed while this waited for its turn (it is removed with
            // this held: `stop_while`): this sentence is passed over, and
            // what follows is said with a voice that is here.
            if !crate::speech::present(dir, model) {
                return Err(SayError::Sentence("the voice is not downloaded".into()));
            }
            // The one before ends before this one loads: one model in
            // memory at a time.
            *running = None;
            *running = Some(Running {
                model: model.id,
                speaker: Speaker::start(&self.program, model, dir).map_err(SayError::Voice)?,
                used: Instant::now(),
            });
        }
        let Some(r) = running.as_mut() else {
            return Err(SayError::Voice("the voice is not running".into()));
        };
        let rate = r.speaker.rate();
        let result = r
            .speaker
            .say(text, lang, voice, speed, &mut |samples| part(samples, rate));
        r.used = Instant::now();
        if matches!(result, Err(SayError::Voice(_))) {
            *running = None;
        }
        result
    }

    /// Let the model go: what is running ends — and with it its hold on
    /// the model's files, which Windows will not delete while it lasts.
    pub fn stop(&self) {
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Let `model` go, if it is the one loaded, and do `work` — remove its
    /// files — with nothing said meanwhile: what is asked to be said waits
    /// until `work` is done. Another model loaded stays: it holds none of
    /// these files, and what it says goes on once `work` is done.
    pub fn stop_while(&self, model: &VoiceModel, work: &mut dyn FnMut()) {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if running.as_ref().is_some_and(|r| r.model == model.id) {
            *running = None;
        }
        work();
    }

    /// Let the model go if nothing has been said with it for `idle`, and
    /// nothing is being said now; whether it was let go.
    pub fn stop_if_idle(&self, idle: Duration) -> bool {
        let Ok(mut running) = self.running.try_lock() else {
            return false;
        };
        if running.as_ref().is_some_and(|r| r.used.elapsed() >= idle) {
            *running = None;
            return true;
        }
        false
    }

    /// Whether a model is loaded.
    pub fn loaded(&self) -> bool {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All `speakers` says of `text`, its parts put together, and its rate.
    #[allow(clippy::too_many_arguments)]
    fn said(
        speakers: &Speakers,
        model: &'static VoiceModel,
        dir: &Path,
        text: &str,
        lang: &str,
        voice: i32,
        speed: f32,
    ) -> Result<(Vec<f32>, u32), SayError> {
        let mut all = (Vec::new(), 0);
        speakers.say(model, dir, text, lang, voice, speed, &mut |part, rate| {
            all.0.extend(part);
            all.1 = rate;
        })?;
        Ok(all)
    }

    /// PCM reads back as the samples it was made of.
    #[test]
    fn pcm_reads_back() {
        let pcm =
            base64::engine::general_purpose::STANDARD.encode([0u8, 0, 0xff, 0x7f, 0x00, 0x80]);
        let samples = decode(&pcm).unwrap();
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0], 0.0);
        assert!((samples[1] - 1.0).abs() < 1e-3);
        assert_eq!(samples[2], -1.0);
    }

    /// Answers read as `clew-speak` writes them.
    #[test]
    fn answers_read_as_written() {
        assert!(matches!(
            serde_json::from_str::<Answer>(r#"{"ready":{"rate":44100,"voices":10}}"#),
            Ok(Answer::Ready {
                rate: 44100,
                voices: 10
            })
        ));
        assert!(matches!(
            serde_json::from_str::<Answer>(r#"{"failed":{"id":2,"error":"no"}}"#),
            Ok(Answer::Failed { id: 2, .. })
        ));
        assert!(matches!(
            serde_json::from_str::<Answer>(r#"{"error":"no model"}"#),
            Ok(Answer::Error(_))
        ));
    }

    /// A folder holding `model`'s files (empty: nothing reads them here).
    fn model_dir(name: &str, model: &VoiceModel) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clew-speaker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for file in model.files {
            let path = dir.join(file.path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        dir
    }

    /// A program that is not there, or that does not answer as a speaker
    /// does, says so — and nothing waits forever.
    #[test]
    fn a_missing_or_silent_speaker_says_so() {
        let model = &crate::speech::VOICE_MODELS[0];
        let dir = model_dir("missing", model);
        let speakers = Speakers::new(PathBuf::from("/nonexistent/clew-speak"));
        let err = said(&speakers, model, &dir, "hi", "en", 0, 1.0).unwrap_err();
        assert!(
            matches!(&err, SayError::Voice(e) if e.contains("could not start the voice")),
            "{err}"
        );
        assert!(!speakers.loaded());
        assert!(!speakers.stop_if_idle(Duration::ZERO));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A voice that streams: its parts are given as they come, in order,
    /// and the sentence ends with its last; a part that cannot be read is
    /// the voice's failure, not the sentence's.
    #[cfg(unix)]
    #[test]
    fn a_streaming_voice_gives_its_parts_as_they_come() {
        use std::os::unix::fs::PermissionsExt;
        let model = &crate::speech::VOICE_MODELS[0];
        let dir = model_dir("stream", model);
        let program = dir.join("clew-speak");
        // Two parts of one sample each (0.5, then -0.5), then the end; then
        // a part that is not base64.
        std::fs::write(
            &program,
            "#!/bin/sh\n\
             echo '{\"ready\":{\"rate\":24000,\"voices\":8}}'\n\
             n=0\n\
             while read -r line; do\n\
               n=$((n+1))\n\
               if [ $n = 1 ]; then\n\
                 echo '{\"audio\":{\"id\":1,\"pcm\":\"AEA=\",\"more\":true}}'\n\
                 echo '{\"audio\":{\"id\":1,\"pcm\":\"AMA=\",\"more\":true}}'\n\
                 echo '{\"audio\":{\"id\":1,\"pcm\":\"\"}}'\n\
               else\n\
                 echo '{\"audio\":{\"id\":2,\"pcm\":\"%%\",\"more\":true}}'\n\
               fi\n\
             done\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let speakers = Speakers::new(program);
        let mut parts = Vec::new();
        speakers
            .say(model, &dir, "Hello.", "en", 0, 1.0, &mut |part, rate| {
                parts.push((part, rate));
            })
            .unwrap();
        assert_eq!(parts, [(vec![0.5], 24_000), (vec![-0.5], 24_000)]);
        let err = said(&speakers, model, &dir, "Again.", "en", 0, 1.0).unwrap_err();
        assert!(matches!(err, SayError::Voice(_)), "{err}");
        assert!(!speakers.loaded(), "a voice that failed is let go");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A model removed while a sentence waited for it (another window
    /// removed it): that sentence is passed over — not the whole answer,
    /// as a voice that failed would be.
    #[test]
    fn a_model_removed_meanwhile_passes_the_sentence_over() {
        let model = &crate::speech::VOICE_MODELS[0];
        let dir = model_dir("removed", model);
        let speakers = Speakers::new(PathBuf::from("/nonexistent/clew-speak"));
        speakers.stop_while(model, &mut || {
            std::fs::remove_dir_all(&dir).unwrap();
        });
        let err = said(&speakers, model, &dir, "hi", "en", 0, 1.0).unwrap_err();
        assert!(matches!(err, SayError::Sentence(_)), "{err}");
        assert!(!speakers.loaded());
    }

    /// A real `clew-speak` and a real model, when the test is pointed at
    /// them: what it says is as long as speech of it is.
    #[test]
    #[ignore = "needs CLEW_SPEAK and CLEW_SPEAK_MODEL"]
    fn a_real_speaker_speaks() {
        let program = PathBuf::from(std::env::var("CLEW_SPEAK").unwrap());
        let dir = PathBuf::from(std::env::var("CLEW_SPEAK_MODEL").unwrap());
        let speakers = Speakers::new(program);
        let model = &crate::speech::VOICE_MODELS[0];
        let (samples, rate) = said(
            &speakers,
            model,
            &dir,
            "The total adds every price.",
            "en",
            0,
            1.0,
        )
        .unwrap();
        let seconds = samples.len() as f32 / rate as f32;
        assert!((1.0..6.0).contains(&seconds), "{seconds}");
        // Again, with the model kept: and in Korean.
        let (samples, _) = said(&speakers, model, &dir, "안녕하세요.", "ko", 3, 1.0).unwrap();
        assert!(!samples.is_empty());
        // Idle long enough, it is let go; asked again, it starts again.
        assert!(speakers.loaded());
        assert!(!speakers.stop_if_idle(Duration::from_secs(600)));
        assert!(speakers.stop_if_idle(Duration::ZERO));
        assert!(!speakers.loaded());
        let (samples, _) = said(&speakers, model, &dir, "Again.", "en", 5, 1.2).unwrap();
        assert!(!samples.is_empty());
    }

    /// A real `clew-speak` and a real model, removed while a sentence
    /// waited for it: that sentence is passed over, not taken for a voice
    /// that failed. The model is not touched: what is removed are links to
    /// its files.
    #[test]
    #[ignore = "needs CLEW_SPEAK and CLEW_SPEAK_MODEL"]
    fn a_real_model_removed_meanwhile_passes_the_sentence_over() {
        let program = PathBuf::from(std::env::var("CLEW_SPEAK").unwrap());
        let model_dir = PathBuf::from(std::env::var("CLEW_SPEAK_MODEL").unwrap());
        let model = &crate::speech::VOICE_MODELS[0];
        let dir = std::env::temp_dir().join(format!("clew-speaker-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // The copy goes with the test, whether or not it passes.
        struct Gone(PathBuf);
        impl Drop for Gone {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _gone = Gone(dir.clone());
        for file in model.files {
            let to = dir.join(file.path);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            // Linked where it can be; copied where the temp folder is on
            // another file system.
            let from = model_dir.join(file.path);
            if std::fs::hard_link(&from, &to).is_err() {
                std::fs::copy(&from, &to).unwrap();
            }
        }
        let speakers = std::sync::Arc::new(Speakers::new(program));
        // Loaded, and saying what it is asked.
        let (loaded, _) = said(&speakers, model, &dir, "Loaded.", "en", 0, 1.0).unwrap();
        assert!(!loaded.is_empty());
        // Another model removed: this one stays loaded.
        speakers.stop_while(&crate::speech::VOICE_MODELS[1], &mut || {});
        assert!(speakers.loaded(), "let go for another model's removal");
        // Removed — and, while it is, a sentence asked for waits its turn.
        let mut waiting = None;
        speakers.stop_while(model, &mut || {
            let (speakers, there) = (speakers.clone(), dir.clone());
            waiting = Some(std::thread::spawn(move || {
                said(&speakers, model, &there, "Waiting.", "en", 0, 1.0)
            }));
            std::thread::sleep(Duration::from_millis(300));
            std::fs::remove_dir_all(&dir).unwrap();
        });
        let err = waiting.unwrap().join().unwrap().unwrap_err();
        assert!(matches!(err, SayError::Sentence(_)), "{err}");
        assert!(!speakers.loaded());
    }
}
