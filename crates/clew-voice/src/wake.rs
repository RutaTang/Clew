//! Waiting to be called: "Hey, Clew" starts listening, heard by
//! `clew-speak listen` — a small keyword spotter, not a speech recognizer.
//!
//! While Clew waits, the microphone is open and what it hears goes to the
//! spotter, piece by piece, and nowhere else: it is neither kept nor worked
//! out into words. Only the phrase, once heard, starts listening.
//!
//! The model is k2-fsa's streaming Zipformer for English and Mandarin,
//! published as one archive; the four files it runs from are taken out of
//! it, each checked against its size and checksum, and the archive let go.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::models::{Checksum, Expected, Install, Store};

/// One file the spotter runs from: its name in the archive and here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeFile {
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// The model, as published: one archive, of which these files are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeModel {
    /// What the settings call it.
    pub name: &'static str,
    /// The archive: where it is, its size and checksum.
    pub url: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    /// The files kept from it.
    pub files: &'static [WakeFile],
    /// Its weights' license.
    pub license: &'static str,
}

/// k2-fsa's keyword spotter for English and Mandarin, 3 million
/// parameters: a 33 MB download, 5.4 MB kept.
pub const MODEL: WakeModel = WakeModel {
    name: "Keyword Spotter",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/kws-models/sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20.tar.bz2",
    bytes: 32_885_699,
    sha256: "68447f4fbc67e70eee3a93961f36e81e98f47aef73ce7e7ca00885c6cd3616a6",
    files: &[
        WakeFile {
            name: "encoder-epoch-13-avg-2-chunk-16-left-64.int8.onnx",
            bytes: 4_599_656,
            sha256: "408bbd740838c42d5bf6d1c5b80b3c88b616c7860b92d980328b5b068c76ae48",
        },
        WakeFile {
            name: "decoder-epoch-13-avg-2-chunk-16-left-64.onnx",
            bytes: 759_829,
            sha256: "63a22dd60f40fff082ac3e09afa507f6787da36df76ded2fbe145fa233e22c21",
        },
        WakeFile {
            name: "joiner-epoch-13-avg-2-chunk-16-left-64.int8.onnx",
            bytes: 86_629,
            sha256: "190d4067b4cc20b72a42a1916e69d92052000fb7051a427ebb1bc72a69207dc1",
        },
        WakeFile {
            name: "tokens.txt",
            bytes: 1_928,
            sha256: "2d3f32311f9b692b964da3c90e830258d3e78e013cb0c992dbfb15cd5a1a71b0",
        },
    ],
    license: "Apache-2.0",
};

/// The phrases listened for, written as the model's sounds (ARPAbet) and
/// named after an `@`. "Hey, Clew" alone: a phrase heard in ordinary speech
/// ("okay, cool") would start listening unasked.
pub const KEYWORDS: &[&str] = &["HH EY1 K L UW1 @hey_clew"];

/// The rate the spotter hears at.
pub const RATE: u32 = 16_000;

impl WakeModel {
    /// Its download size, in MB: shown before anything is fetched.
    pub fn size_mb(&self) -> u32 {
        self.bytes.div_ceil(1_000_000) as u32
    }

    /// What a download of the archive must turn out to be.
    pub fn expected(&self) -> Expected {
        Expected {
            // As it is published: the archive's own name.
            name: self.url.rsplit('/').next().unwrap_or(self.url).to_string(),
            bytes: Some(self.bytes),
            max_bytes: self.bytes,
            checksum: Checksum::Sha256(self.sha256),
        }
    }
}

/// Where the model is kept in `store`: a folder of its own.
pub fn dir(store: &Store) -> PathBuf {
    store.dir().join("wake")
}

/// Where the archive is downloaded to, before its files are taken out.
pub fn archive(store: &Store) -> PathBuf {
    dir(store).join("model.tar.bz2")
}

/// Whether the model is in `store`: every file it runs from, whole.
pub fn installed(store: &Store) -> bool {
    MODEL.files.iter().all(|f| {
        std::fs::metadata(dir(store).join(f.name)).is_ok_and(|m| m.is_file() && m.len() == f.bytes)
    })
}

/// Start a download of the archive into `store`.
pub fn begin(store: &Store) -> std::io::Result<Install> {
    std::fs::create_dir_all(dir(store))?;
    Install::begin(archive(store), MODEL.expected())
}

/// Take the files the spotter runs from out of the downloaded archive in
/// `store`, each checked, then let the archive go.
pub fn unpack(store: &Store) -> Result<(), String> {
    let file = std::fs::File::open(archive(store))
        .map_err(|e| format!("the download could not be opened: {e}"))?;
    let result = unpack_from(file, &dir(store), MODEL.files);
    let _ = std::fs::remove_file(archive(store));
    result
}

/// Take `files` out of the `.tar.bz2` read from `archive` into `dir`: each
/// written beside its name, checked against its size and checksum, then
/// put in place. Anything else in the archive is passed over.
pub fn unpack_from(archive: impl Read, dir: &Path, files: &[WakeFile]) -> Result<(), String> {
    let mut found = vec![false; files.len()];
    let mut tar = tar::Archive::new(bzip2::read::BzDecoder::new(archive));
    let entries = tar
        .entries()
        .map_err(|e| format!("the download is not an archive: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("the archive is damaged: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("the archive is damaged: {e}"))?
            .into_owned();
        // A file at the archive's top folder, by its name alone: nothing is
        // written anywhere the archive says but here.
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(i) = files.iter().position(|f| f.name == name) else {
            continue;
        };
        let want = files[i];
        if entry.size() != want.bytes {
            return Err(format!(
                "{name} in the archive is not the size it should be"
            ));
        }
        let part = dir.join(format!("{name}.part"));
        let mut out =
            std::fs::File::create(&part).map_err(|e| format!("could not save the model: {e}"))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = entry
                .read(&mut buf)
                .map_err(|e| format!("the archive is damaged: {e}"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .map_err(|e| format!("could not save the model: {e}"))?;
        }
        out.sync_all()
            .map_err(|e| format!("could not save the model: {e}"))?;
        drop(out);
        let got: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if got != want.sha256 {
            let _ = std::fs::remove_file(&part);
            return Err(format!(
                "{name} in the archive is not the file it should be"
            ));
        }
        std::fs::rename(&part, dir.join(name))
            .map_err(|e| format!("could not save the model: {e}"))?;
        found[i] = true;
    }
    match files.iter().zip(&found).find(|(_, found)| !**found) {
        Some((missing, _)) => Err(format!("the archive has no {}", missing.name)),
        None => Ok(()),
    }
}

/// Remove the model from `store`. Windows deletes no file while it is open,
/// and a `clew-speak` letting go of it takes a moment: the removal is tried
/// again a few times before it is given up on.
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
/// What `clew-speak` exits with when it does not know what it was asked to
/// run.
const USAGE: i32 = 2;

/// What `clew-speak listen` answers.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Answer {
    Ready {
        #[allow(dead_code)]
        keywords: usize,
    },
    Heard {
        #[allow(dead_code)]
        keyword: String,
    },
    Error(String),
}

/// One `clew-speak listen`, with the model loaded.
pub struct Spotter {
    child: Child,
    stdin: ChildStdin,
    answers: mpsc::Receiver<Result<Answer, String>>,
}

impl Spotter {
    /// Start `program` with the model in `dir`, listening for [`KEYWORDS`],
    /// and wait for it to have loaded it.
    pub fn start(program: &Path, dir: &Path) -> Result<Spotter, String> {
        let mut command = Command::new(program);
        command
            .arg("listen")
            .arg(dir)
            .args(KEYWORDS)
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
            .map_err(|e| format!("could not start the keyword spotter: {e}"))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("could not connect to the keyword spotter".into());
        };
        let (tx, answers) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("clew-speak-listen".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let answer = match line {
                        Ok(line) => serde_json::from_str::<Answer>(&line).map_err(|e| {
                            format!("the keyword spotter said something unreadable: {e}")
                        }),
                        Err(e) => Err(format!(
                            "the reply from the keyword spotter could not be read: {e}"
                        )),
                    };
                    let stop = answer.is_err();
                    if tx.send(answer).is_err() || stop {
                        break;
                    }
                }
            });
        let mut spotter = Spotter {
            child,
            stdin,
            answers,
        };
        if let Err(e) = reader {
            return Err(format!("could not start the keyword spotter: {e}"));
        }
        match spotter.answers.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(Answer::Ready { .. })) => Ok(spotter),
            Ok(Ok(Answer::Error(e))) | Ok(Err(e)) => Err(e),
            Ok(Ok(Answer::Heard { .. })) => {
                Err("the keyword spotter answered before it was ready".into())
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err("the keyword spotter took too long to load".into())
            }
            // A `clew-speak` older than this Clew does not know `listen`.
            Err(mpsc::RecvTimeoutError::Disconnected)
                if spotter.child.wait().ok().and_then(|s| s.code()) == Some(USAGE) =>
            {
                Err("the clew-speak beside Clew is older than it and cannot listen for the wake phrase".into())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("the keyword spotter stopped while starting — it may be damaged".into())
            }
        }
    }

    /// Hand it `samples` ([`RATE`], one channel), heard after the last; and
    /// whether the phrase has been heard since it was last asked.
    pub fn hear(&mut self, samples: &[f32]) -> Result<bool, String> {
        if !samples.is_empty() {
            let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
            let audio = base64::engine::general_purpose::STANDARD.encode(bytes);
            let request = serde_json::json!({ "hear": { "audio": audio } });
            writeln!(self.stdin, "{request}")
                .and_then(|()| self.stdin.flush())
                .map_err(|e| format!("the keyword spotter stopped taking audio: {e}"))?;
        }
        let mut heard = false;
        loop {
            match self.answers.try_recv() {
                Ok(Ok(Answer::Heard { .. })) => heard = true,
                Ok(Ok(Answer::Ready { .. })) => {}
                Ok(Ok(Answer::Error(e))) | Ok(Err(e)) => return Err(e),
                Err(mpsc::TryRecvError::Empty) => return Ok(heard),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("the keyword spotter ended".into());
                }
            }
        }
    }

    /// Let go of what it has heard so far.
    pub fn reset(&mut self) -> Result<(), String> {
        writeln!(self.stdin, "{}", serde_json::json!({ "reset": {} }))
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("the keyword spotter stopped taking audio: {e}"))
    }

    /// Whether it has ended.
    fn ended(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Spotter {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The `clew-speak listen` running, if one is.
pub struct Spotters {
    program: PathBuf,
    running: Mutex<Option<(Spotter, PathBuf)>>,
}

impl Spotters {
    /// Spotters run as `program`.
    pub fn new(program: PathBuf) -> Self {
        Self {
            program,
            running: Mutex::new(None),
        }
    }

    /// Start one with the model in `dir`, if none is running with it: it
    /// loads in a moment, off the thread that hands it audio.
    pub fn warm(&self, dir: &Path) -> Result<(), String> {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((spotter, at)) = running.as_mut()
            && !spotter.ended()
            && at == dir
        {
            return Ok(());
        }
        *running = None;
        *running = Some((Spotter::start(&self.program, dir)?, dir.to_path_buf()));
        Ok(())
    }

    /// Hand `samples` ([`RATE`]) to the spotter with the model in `dir`, and
    /// whether the phrase was heard — `None` while none is running with it,
    /// or one is loading: never waited for ([`Spotters::warm`] starts one).
    pub fn hear(&self, dir: &Path, samples: &[f32]) -> Result<Option<bool>, String> {
        let Ok(mut running) = self.running.try_lock() else {
            return Ok(None);
        };
        let Some((spotter, at)) = running.as_mut() else {
            return Ok(None);
        };
        if at != dir || spotter.ended() {
            *running = None;
            return Ok(None);
        }
        let result = spotter.hear(samples);
        if result.is_err() {
            *running = None;
        }
        result.map(Some)
    }

    /// Let go of what the spotter has heard so far, if one is running.
    pub fn reset(&self) {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((spotter, _)) = running.as_mut()
            && spotter.reset().is_err()
        {
            *running = None;
        }
    }

    /// Let the model go: what is running ends, and with it its hold on the
    /// model's files.
    pub fn stop(&self) {
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// What the microphone hears, made into pieces for the spotter: at its
/// [`RATE`], each [`FEED_SECONDS`] long — a piece resampled whole, not a
/// tick's worth at a time, so its edges are few.
#[derive(Debug, Default)]
pub struct Feed {
    pending: Vec<f32>,
    rate: u32,
}

/// How long each piece handed to the spotter is.
pub const FEED_SECONDS: f32 = 0.2;

impl Feed {
    /// `samples`, heard at `rate`, after what came before: a piece for the
    /// spotter once there is one.
    pub fn push(&mut self, samples: &[f32], rate: u32) -> Option<Vec<f32>> {
        if rate != self.rate {
            self.pending.clear();
            self.rate = rate;
        }
        self.pending.extend_from_slice(samples);
        let piece = (rate as f32 * FEED_SECONDS) as usize;
        if rate == 0 || self.pending.len() < piece.max(1) {
            return None;
        }
        let heard = std::mem::take(&mut self.pending);
        Some(crate::audio::resample(&heard, rate, RATE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `.tar.bz2` of `files` (name, contents) under one top folder.
    fn archive_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, format!("model/{name}"), *data)
                .unwrap();
        }
        let tar = tar.into_inner().unwrap();
        let mut out = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
        out.write_all(&tar).unwrap();
        out.finish().unwrap()
    }

    fn sha256(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clew-wake-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The files it runs from are taken out of the archive, checked, and
    /// put in place; the rest of it is passed over.
    #[test]
    fn the_model_is_taken_out_of_its_archive() {
        let (a, b) = (b"encoder weights".as_slice(), b"<blk> 0\n".as_slice());
        let hash_a = sha256(a);
        let hash_b = sha256(b);
        let files = [
            WakeFile {
                name: "a.onnx",
                bytes: a.len() as u64,
                sha256: Box::leak(hash_a.into_boxed_str()),
            },
            WakeFile {
                name: "tokens.txt",
                bytes: b.len() as u64,
                sha256: Box::leak(hash_b.into_boxed_str()),
            },
        ];
        let dir = temp_dir("unpack");
        let archive = archive_of(&[("a.onnx", a), ("test.wav", b"riff"), ("tokens.txt", b)]);
        unpack_from(archive.as_slice(), &dir, &files).unwrap();
        assert_eq!(std::fs::read(dir.join("a.onnx")).unwrap(), a);
        assert_eq!(std::fs::read(dir.join("tokens.txt")).unwrap(), b);
        assert!(
            !dir.join("test.wav").exists(),
            "what it does not run from is left"
        );
        // A file that is not the one it should be is not put in place.
        let dir = temp_dir("unpack-bad");
        let archive = archive_of(&[("a.onnx", b"encoder weightz"), ("tokens.txt", b)]);
        let e = unpack_from(archive.as_slice(), &dir, &files).unwrap_err();
        assert!(e.contains("a.onnx"), "{e}");
        assert!(!dir.join("a.onnx").exists());
        assert!(!dir.join("a.onnx.part").exists());
        // One missing is said.
        let dir = temp_dir("unpack-missing");
        let archive = archive_of(&[("a.onnx", a)]);
        let e = unpack_from(archive.as_slice(), &dir, &files).unwrap_err();
        assert!(e.contains("tokens.txt"), "{e}");
    }

    /// What is heard goes to the spotter in pieces of a fifth of a second,
    /// at its rate, whatever the microphone's.
    #[test]
    fn what_is_heard_is_fed_in_pieces_at_the_spotters_rate() {
        let mut feed = Feed::default();
        // 60 ms at 48 kHz, three times: not yet a piece.
        for _ in 0..3 {
            assert_eq!(feed.push(&[0.1; 2_880], 48_000), None);
        }
        // A fourth: 240 ms, made into 16 kHz.
        let piece = feed.push(&[0.1; 2_880], 48_000).expect("a piece");
        assert_eq!(piece.len(), 3_840);
        assert_eq!(
            feed.push(&[0.1; 1_600], 16_000),
            None,
            "a new rate starts afresh"
        );
        assert_eq!(
            feed.push(&[0.1; 1_600], 16_000).map(|p| p.len()),
            Some(3_200)
        );
    }

    /// The archive is fetched from k2-fsa's release, held to its size and
    /// checksum; the phrase is written in sounds the model knows.
    #[test]
    fn the_model_is_pinned_and_the_phrase_is_in_its_sounds() {
        assert!(
            MODEL
                .url
                .starts_with("https://github.com/k2-fsa/sherpa-onnx/releases/")
        );
        let expected = MODEL.expected();
        assert_eq!(expected.bytes, Some(32_885_699));
        assert_eq!(
            expected.name,
            "sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20.tar.bz2"
        );
        assert_eq!(MODEL.size_mb(), 33);
        // English sounds of ARPAbet, with their stress, as the model's
        // tokens are written; then the phrase's name.
        const ARPABET: &[&str] = &[
            "AA", "AE", "AH", "AO", "AW", "AY", "B", "CH", "D", "DH", "EH", "ER", "EY", "F", "G",
            "HH", "IH", "IY", "JH", "K", "L", "M", "N", "NG", "OW", "OY", "P", "R", "S", "SH", "T",
            "TH", "UH", "UW", "V", "W", "Y", "Z", "ZH",
        ];
        for keyword in KEYWORDS {
            let (sounds, name) = keyword.split_once(" @").expect("named");
            assert!(!name.is_empty() && !name.contains(' '));
            for sound in sounds.split_whitespace() {
                let base = sound.trim_end_matches(['0', '1', '2']);
                assert!(ARPABET.contains(&base), "{sound}");
            }
        }
    }

    /// The real model, run by the real `clew-speak`, hears "Hey, Clew" and
    /// not other speech. Needs `CLEW_SPEAK` (the program), `CLEW_WAKE_MODEL`
    /// (the folder of its files) and `CLEW_WAKE_WAVS` (a folder of
    /// `hey*.wav` and `other*.wav`, 16 kHz, 16-bit, one channel).
    #[test]
    #[ignore = "needs clew-speak, the model and recordings"]
    fn the_real_model_hears_the_phrase() {
        let (Some(program), Some(model), Some(wavs)) = (
            std::env::var_os("CLEW_SPEAK"),
            std::env::var_os("CLEW_WAKE_MODEL"),
            std::env::var_os("CLEW_WAKE_WAVS"),
        ) else {
            return;
        };
        let read = |path: &Path| -> Vec<f32> {
            let bytes = std::fs::read(path).unwrap();
            bytes[44..]
                .chunks_exact(2)
                .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
                .collect()
        };
        let mut hits = (0, 0);
        let mut false_wakes = (0, 0);
        let mut names: Vec<_> = std::fs::read_dir(&wavs)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        for path in names {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let mut spotter = Spotter::start(Path::new(&program), Path::new(&model)).unwrap();
            let mut samples = vec![0.0; RATE as usize / 2];
            samples.extend(read(&path));
            samples.extend(vec![0.0; RATE as usize]);
            let mut heard = false;
            for piece in samples.chunks(1_600) {
                heard |= spotter.hear(piece).unwrap();
            }
            std::thread::sleep(Duration::from_millis(300));
            heard |= spotter.hear(&[]).unwrap();
            if name.starts_with("hey") {
                hits = (hits.0 + usize::from(heard), hits.1 + 1);
            } else if name.starts_with("other") {
                false_wakes = (false_wakes.0 + usize::from(heard), false_wakes.1 + 1);
            }
        }
        eprintln!(
            "heard {}/{}, woken by other speech {}/{}",
            hits.0, hits.1, false_wakes.0, false_wakes.1
        );
        assert!(hits.0 * 10 >= hits.1 * 9, "at least nine in ten heard");
        assert_eq!(false_wakes.0, 0, "never woken by other speech");
    }
}
