//! When a speaker has finished: Smart Turn, run by `clew-speak turn`.
//!
//! A pause is not always the end of what someone means to say — they stop
//! to think, mid-sentence — and waiting a long pause out before every
//! answer makes each one slow. Smart Turn hears how the last words were
//! said and answers how likely it is that the speaker is done, so a
//! finished sentence is acted on at once and a pause to think is waited
//! through.
//!
//! [`Judges`] keeps one `clew-speak turn` running with the model loaded,
//! and starts it again if it ended.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Deserialize;

use crate::models::{Checksum, Expected, Install, Store};

/// The model, as published: one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnModel {
    /// What the settings call it.
    pub name: &'static str,
    /// The Hugging Face repository it is fetched from, and the revision.
    pub repo: &'static str,
    pub revision: &'static str,
    /// Its file, upstream and here.
    pub file: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    /// Its weights' license.
    pub license: &'static str,
}

/// Smart Turn v3.2, for the CPU (int8): 8 MB; a pause is judged in about a
/// tenth of a second, most of it making the spectrogram the model hears.
pub const MODEL: TurnModel = TurnModel {
    name: "Smart Turn",
    repo: "pipecat-ai/smart-turn-v3",
    revision: "f766f81d3cfdf7737ac64aad813d91bbfd56bf93",
    file: "smart-turn-v3.2-cpu.onnx",
    bytes: 8_679_182,
    sha256: "2bb026316b14a660486a75b1733cd3fbab8c2fd0314dc9af7be49f8cca967e4f",
    license: "BSD-2-Clause",
};

/// The rate the audio it hears is at.
pub const RATE: u32 = 16_000;
/// How much of what was said it hears: the last eight seconds.
pub const SECONDS: f32 = 8.0;

impl TurnModel {
    /// Its download size, in MB: shown before anything is fetched.
    pub fn size_mb(&self) -> u32 {
        self.bytes.div_ceil(1_000_000) as u32
    }

    /// Where it is published, at the pinned revision.
    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo, self.revision, self.file
        )
    }

    /// What a download of it must turn out to be.
    pub fn expected(&self) -> Expected {
        Expected {
            name: self.file.to_string(),
            bytes: Some(self.bytes),
            max_bytes: self.bytes,
            checksum: Checksum::Sha256(self.sha256),
        }
    }
}

/// Where the model is kept in `store`: a folder of its own, beside the
/// speech models'.
pub fn dir(store: &Store) -> PathBuf {
    store.dir().join("turn")
}

/// The model's file in `store`.
pub fn path(store: &Store) -> PathBuf {
    dir(store).join(MODEL.file)
}

/// Whether the model is in `store`.
pub fn installed(store: &Store) -> bool {
    std::fs::metadata(path(store)).is_ok_and(|m| m.is_file() && m.len() == MODEL.bytes)
}

/// Start a download of the model into `store`.
pub fn begin(store: &Store) -> std::io::Result<Install> {
    std::fs::create_dir_all(dir(store))?;
    Install::begin(path(store), MODEL.expected())
}

/// Remove the model from `store`. A `clew-speak` loading it reads it in and
/// lets go within moments, and Windows deletes no file while it is open:
/// the removal is tried again a few times before it is given up on.
pub fn remove(store: &Store) -> std::io::Result<()> {
    let mut last = Ok(());
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(400));
        }
        last = match std::fs::remove_dir_all(dir(store)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        };
        if last.is_ok() {
            break;
        }
    }
    last
}

/// How long loading the model may take.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long one judgement may take: past this, the pause is waited out as
/// it would be without the model.
const JUDGE_TIMEOUT: Duration = Duration::from_secs(2);
/// What `clew-speak` exits with when it does not know what it was asked to
/// run.
const USAGE: i32 = 2;

/// Why a pause was not judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// This pause: what judges it is fine, and judges the next.
    Pause(String),
    /// What judges: it ended, or stopped answering — started afresh for the
    /// next pause.
    Process(String),
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fault::Pause(e) | Fault::Process(e) => f.write_str(e),
        }
    }
}

/// What `clew-speak turn` answers.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Answer {
    Ready {
        #[allow(dead_code)]
        seconds: usize,
    },
    Done {
        id: u64,
        p: f32,
    },
    Failed {
        id: u64,
        error: String,
    },
    Error(String),
}

/// One `clew-speak turn`, with the model loaded.
pub struct Judge {
    child: Child,
    stdin: ChildStdin,
    answers: mpsc::Receiver<Result<Answer, String>>,
    next: u64,
}

impl Judge {
    /// Start `program` with the model in `file`, and wait for it to have
    /// loaded it.
    pub fn start(program: &Path, file: &Path) -> Result<Judge, String> {
        let mut command = Command::new(program);
        command
            .arg("turn")
            .arg(file)
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
            .map_err(|e| format!("could not start {}: {e}", MODEL.name))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("could not connect to {}", MODEL.name));
        };
        let (tx, answers) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("clew-speak-turn".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let answer = match line {
                        Ok(line) => serde_json::from_str::<Answer>(&line)
                            .map_err(|e| format!("{} said something unreadable: {e}", MODEL.name)),
                        Err(e) => Err(format!(
                            "the reply from {} could not be read: {e}",
                            MODEL.name
                        )),
                    };
                    let stop = answer.is_err();
                    if tx.send(answer).is_err() || stop {
                        break;
                    }
                }
            });
        let mut judge = Judge {
            child,
            stdin,
            answers,
            next: 0,
        };
        if let Err(e) = reader {
            return Err(format!("could not start {}: {e}", MODEL.name));
        }
        match judge.answers.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(Answer::Ready { .. })) => Ok(judge),
            Ok(Ok(Answer::Error(e))) | Ok(Err(e)) => Err(e),
            Ok(Ok(_)) => Err(format!("{} answered before it was ready", MODEL.name)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("{} took too long to load", MODEL.name))
            }
            // A `clew-speak` older than this Clew does not know `turn`: it
            // says how it is used and ends, as it does for any word it does
            // not know. Its model is fine.
            Err(mpsc::RecvTimeoutError::Disconnected)
                if judge.child.wait().ok().and_then(|s| s.code()) == Some(USAGE) =>
            {
                Err(format!(
                    "the clew-speak beside Clew is older than it and cannot run {}",
                    MODEL.name
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(format!(
                "{} stopped while starting — its model may be damaged",
                MODEL.name
            )),
        }
    }

    /// How likely it is, 0 to 1, that the speaker of `samples` ([`RATE`],
    /// one channel) has finished.
    pub fn judge(&mut self, samples: &[f32]) -> Result<f32, Fault> {
        use Fault::{Pause, Process};
        self.next += 1;
        let id = self.next;
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let audio = base64::engine::general_purpose::STANDARD.encode(bytes);
        let request = serde_json::json!({ "turn": { "id": id, "audio": audio } });
        writeln!(self.stdin, "{request}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| Process(format!("{} stopped taking audio: {e}", MODEL.name)))?;
        let deadline = Instant::now() + JUDGE_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.answers.recv_timeout(left) {
                Ok(Ok(Answer::Done { id: got, p })) if got == id => {
                    return if p.is_finite() {
                        Ok(p.clamp(0.0, 1.0))
                    } else {
                        Err(Pause(format!(
                            "{} answered {p}, not a probability",
                            MODEL.name
                        )))
                    };
                }
                // This pause: the next is judged by the same, still loaded.
                Ok(Ok(Answer::Failed { id: got, error })) if got == id => return Err(Pause(error)),
                // An answer to a question given up on.
                Ok(Ok(Answer::Done { .. } | Answer::Failed { .. } | Answer::Ready { .. })) => {}
                Ok(Ok(Answer::Error(e))) | Ok(Err(e)) => return Err(Process(e)),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Not waited for: the pause is waited out without it,
                    // and a fresh one starts for the next.
                    let _ = self.child.kill();
                    return Err(Process(format!("{} took too long to answer", MODEL.name)));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Process(format!(
                        "{} ended while it was judging a pause",
                        MODEL.name
                    )));
                }
            }
        }
    }

    /// Whether it has ended.
    fn ended(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Judge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The `clew-speak turn` running, if one is.
pub struct Judges {
    program: PathBuf,
    running: Mutex<Option<(Judge, Instant)>>,
}

impl Judges {
    /// Judges run as `program`.
    pub fn new(program: PathBuf) -> Self {
        Self {
            program,
            running: Mutex::new(None),
        }
    }

    /// Start one with the model in `file`, if none is running: so the first
    /// pause is judged at once, not after the model loads.
    pub fn warm(&self, file: &Path) -> Result<(), String> {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((judge, used)) = running.as_mut()
            && !judge.ended()
        {
            // Wanted now: not let go of as idle a moment before it is asked.
            *used = Instant::now();
            return Ok(());
        }
        *running = None;
        *running = Some((Judge::start(&self.program, file)?, Instant::now()));
        Ok(())
    }

    /// How likely it is that the speaker of `samples` has finished, by the
    /// model in `file` — started if it is not running, or ended.
    pub fn judge(&self, file: &Path, samples: &[f32]) -> Result<f32, String> {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if running.as_mut().is_none_or(|(j, _)| j.ended()) {
            *running = None;
            *running = Some((Judge::start(&self.program, file)?, Instant::now()));
        }
        let Some((judge, used)) = running.as_mut() else {
            return Err(format!("{} is not running", MODEL.name));
        };
        let result = judge.judge(samples);
        *used = Instant::now();
        if matches!(result, Err(Fault::Process(_))) {
            *running = None;
        }
        result.map_err(|e| e.to_string())
    }

    /// Let the model go: what is running ends — and with it its hold on
    /// the model's file, which Windows will not delete while it lasts.
    pub fn stop(&self) {
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Let the model go if it has not been asked anything for `idle`, and
    /// is not being asked now; whether it was let go.
    pub fn stop_if_idle(&self, idle: Duration) -> bool {
        let Ok(mut running) = self.running.try_lock() else {
            return false;
        };
        if running
            .as_ref()
            .is_some_and(|(_, used)| used.elapsed() >= idle)
        {
            *running = None;
            return true;
        }
        false
    }

    /// Whether the model is loaded.
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

    /// The model is fetched from the revision it was checked at, and a
    /// download is held to its size and checksum.
    #[test]
    fn the_model_is_pinned() {
        assert_eq!(
            MODEL.url(),
            "https://huggingface.co/pipecat-ai/smart-turn-v3/resolve/\
             f766f81d3cfdf7737ac64aad813d91bbfd56bf93/smart-turn-v3.2-cpu.onnx"
        );
        let expected = MODEL.expected();
        assert_eq!(expected.bytes, Some(8_679_182));
        assert_eq!(expected.max_bytes, 8_679_182);
        assert_eq!(MODEL.size_mb(), 9);
    }

    /// The real model, run by the real `clew-speak`, hears a finished
    /// sentence as finished and one broken off mid-thought as not. Needs
    /// `CLEW_SPEAK` (the program), `CLEW_TURN_MODEL` (the model file) and
    /// `CLEW_TURN_WAVS` (a folder of `done*.wav` and `cut*.wav`).
    #[test]
    #[ignore = "needs the model, clew-speak and recordings; run explicitly"]
    fn a_real_model_hears_a_finished_turn() {
        let var = |name: &str| PathBuf::from(std::env::var_os(name).expect(name));
        let (program, model, wavs) = (
            var("CLEW_SPEAK"),
            var("CLEW_TURN_MODEL"),
            var("CLEW_TURN_WAVS"),
        );
        let judges = Judges::new(program);
        judges.warm(&model).unwrap();
        let mut heard = 0;
        for entry in std::fs::read_dir(&wavs).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let finished = if name.starts_with("done") {
                true
            } else if name.starts_with("cut") {
                false
            } else {
                continue;
            };
            let (samples, rate) = crate::wav::read(&std::fs::read(&path).unwrap()).unwrap();
            let samples = crate::audio::resample(&samples, rate, RATE);
            let began = Instant::now();
            let p = judges.judge(&model, &samples).unwrap();
            eprintln!("{name}: {p:.3} in {:?}", began.elapsed());
            if finished {
                assert!(p >= 0.5, "{name} heard unfinished: {p}");
            } else {
                assert!(p < 0.5, "{name} heard finished: {p}");
            }
            heard += 1;
        }
        assert!(heard >= 2, "no recordings in {}", wavs.display());
        judges.stop();
        assert!(!judges.loaded());
    }

    /// A folder removed when the test is done with it.
    #[cfg(unix)]
    struct Scratch(PathBuf);

    #[cfg(unix)]
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A program standing in for `clew-speak turn` — `script` — in a
    /// folder of its own, and where a model would be.
    #[cfg(unix)]
    fn stand_in(tag: &str, script: &str) -> (Scratch, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("clew-turn-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("clew-speak");
        std::fs::write(&program, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let model = dir.join("model.onnx");
        (Scratch(dir), program, model)
    }

    /// Loading the model for a listening counts as using it: it is not let
    /// go of as idle a moment before the first pause is judged.
    #[cfg(unix)]
    #[test]
    fn warming_counts_as_use() {
        let (_dir, program, model) = stand_in(
            "warm",
            "printf '{\"ready\":{\"seconds\":8}}\\n'; cat >/dev/null",
        );
        let judges = Judges::new(program);
        judges.warm(&model).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        judges.warm(&model).unwrap();
        assert!(!judges.stop_if_idle(Duration::from_millis(60)), "let go");
        assert!(judges.loaded());
        judges.stop();
    }

    /// A pause it could not judge leaves it loaded for the next; one that
    /// ends is started afresh.
    #[cfg(unix)]
    #[test]
    fn a_failed_pause_keeps_what_judges() {
        let (_dir, program, model) = stand_in(
            "failed",
            "printf '{\"ready\":{\"seconds\":8}}\\n'; \
             while read -r line; do \
               id=$(printf '%s' \"$line\" | sed 's/.*\"id\":\\([0-9]*\\).*/\\1/'); \
               printf '{\"failed\":{\"id\":%s,\"error\":\"no answer\"}}\\n' \"$id\"; \
             done",
        );
        let judges = Judges::new(program);
        assert_eq!(judges.judge(&model, &[0.0; 16]), Err("no answer".into()));
        assert!(judges.loaded(), "still loaded for the next pause");
        assert_eq!(judges.judge(&model, &[0.0; 16]), Err("no answer".into()));
        judges.stop();
    }

    /// A `clew-speak` too old to know `turn` is said to be that — not a
    /// damaged model, which would only be downloaded again for nothing.
    #[cfg(unix)]
    #[test]
    fn an_older_clew_speak_is_said_to_be_older() {
        let (_dir, program, model) = stand_in(
            "old",
            "echo 'usage: clew-speak supertonic|kokoro <model dir>' >&2; exit 2",
        );
        let error = Judge::start(&program, &model).err().unwrap();
        assert!(error.contains("older than it"), "{error}");
        assert!(!error.contains("damaged"), "{error}");
    }

    /// Kept in a folder of its own in the store, and only a whole file of
    /// the right size counts as installed.
    #[test]
    fn it_is_installed_only_whole() {
        let root = std::env::temp_dir().join(format!("clew-turn-{}", std::process::id()));
        let store = Store::new(&root);
        assert!(!installed(&store));
        std::fs::create_dir_all(dir(&store)).unwrap();
        std::fs::write(path(&store), b"part").unwrap();
        assert!(!installed(&store), "a partial file is not the model");
        remove(&store).unwrap();
        assert!(!dir(&store).exists());
        remove(&store).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
