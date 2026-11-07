//! Embeddings made on this computer, with no service and no key:
//! EmbeddingGemma (Google DeepMind, 308M parameters, the Gemma license),
//! quantised to 4 bits as onnx-community publishes it, and run by the
//! `clew-speak embed` program beside Clew.
//!
//! The model is fetched once, when asked for, into Clew's data directory —
//! pinned to one revision, each file checked against the size and SHA-256 it
//! was published with. Once loaded it is kept for the next batch, and let go
//! after [`UNLOAD_AFTER`] of quiet. Loaded, it is a copy in memory: no
//! process holds its files, so they can be removed whoever has it loaded —
//! Clew, or the server beside it.
//!
//! EmbeddingGemma is told what each text is for: what is indexed is a
//! document, what is searched for a query ([`Purpose`]), each with the
//! prompt the model was trained with.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Mutex, MutexGuard, TryLockError, mpsc};
use std::time::{Duration, Instant};

use base64::Engine as _;

/// What an embedding config's `base_url` says when its vectors are made on
/// this computer.
pub const BASE_URL: &str = "local";

/// One file of the model, as published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFile {
    /// Where it is in the model's folder, upstream and here.
    pub path: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// The model embeddings are made with here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalModel {
    /// Its id: the `model` of a local embedding config, and its folder.
    pub id: &'static str,
    /// What the settings call it.
    pub name: &'static str,
    /// The Hugging Face repository it is fetched from, and the revision.
    pub repo: &'static str,
    pub revision: &'static str,
    pub files: &'static [ModelFile],
    /// Its weights' license, and where its terms are.
    pub license: &'static str,
    pub terms: &'static str,
}

/// EmbeddingGemma 300M, 4-bit: 768 dimensions, 2,048 tokens of context,
/// a hundred languages, and code.
pub const MODEL: LocalModel = LocalModel {
    id: "embeddinggemma-300m",
    name: "EmbeddingGemma",
    repo: "onnx-community/embeddinggemma-300m-ONNX",
    revision: "5090578d9565bb06545b4552f76e6bc2c93e4a66",
    files: &[
        ModelFile {
            path: "onnx/model_q4.onnx",
            bytes: 519_322,
            sha256: "ad1dfee81a70f7944b9b9d1cc6e48075b832881cf33fab2f2b248be78f3f0043",
        },
        ModelFile {
            path: "onnx/model_q4.onnx_data",
            bytes: 196_725_760,
            sha256: "599962c3143b040de2dd05e5975be3e9091dd067cacc6a8f7186e3203bab9e02",
        },
        ModelFile {
            path: "tokenizer.json",
            bytes: 20_323_312,
            sha256: "4dda02faaf32bc91031dc8c88457ac272b00c1016cc679757d1c441b248b9c47",
        },
    ],
    license: "Gemma",
    terms: "https://ai.google.dev/gemma/terms",
};

impl LocalModel {
    /// Its total size, in bytes.
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }

    /// Its download size, in MB: shown before anything is fetched.
    pub fn size_mb(&self) -> u32 {
        (self.bytes() / 1_000_000) as u32
    }

    /// Where `file` is published, at the pinned revision.
    pub fn url(&self, file: &ModelFile) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo, self.revision, file.path
        )
    }
}

/// Where the model is kept: `<data directory>/embed/<id>`.
pub fn dir() -> Option<PathBuf> {
    crate::lsp::store::data_root().map(|root| root.join("embed").join(MODEL.id))
}

/// Whether every file of the model is in `dir`. Only a verified download
/// is ever renamed to its name, so each file's presence is enough.
pub fn present(dir: &Path) -> bool {
    MODEL
        .files
        .iter()
        .all(|f| std::fs::symlink_metadata(dir.join(f.path)).is_ok_and(|m| m.is_file()))
}

/// Whether the model is here.
pub fn installed() -> bool {
    dir().is_some_and(|dir| present(&dir))
}

/// `clew-speak`, which runs the model: beside the running program — Clew,
/// or its server, as the archives ship them and Cargo builds them — else on
/// `PATH`, by absolute path only.
pub fn program() -> Option<PathBuf> {
    const NAME: &str = "clew-speak";
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(format!("{NAME}{}", std::env::consts::EXE_SUFFIX));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    crate::lsp::store::find_on_path(NAME)
}

/// Whether embeddings can be made here: the model downloaded, and the
/// program that runs it beside Clew.
pub fn ready() -> bool {
    installed() && program().is_some()
}

/// What a text is embedded for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Kept in the index, to be found.
    Document,
    /// Searched for.
    Query,
}

/// `text` with the prompt EmbeddingGemma was trained to read before a
/// text of `purpose`.
pub fn prompted(purpose: Purpose, text: &str) -> String {
    let prompt = match purpose {
        Purpose::Document => "title: none | text: ",
        Purpose::Query => "task: search result | query: ",
    };
    [prompt, text].concat()
}

/// How long the model may take to load: a cold disk, a slow computer.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// How long one batch may take before the model is taken for stuck.
const BATCH_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the model is kept loaded with nothing asked of it.
pub const UNLOAD_AFTER: Duration = Duration::from_secs(5 * 60);

/// Texts embedded at once: few enough that a stop is heard within a
/// batch's time, and that the longest text in one does not pad many.
const BATCH: usize = 16;

/// How often a wait re-tests whether it is still wanted.
const POLL: Duration = Duration::from_millis(50);

/// What `clew-speak embed` says.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Answer {
    Ready { dims: usize },
    Vectors { id: u64, dims: usize, data: String },
    Failed { id: u64, error: String },
    Error(String),
}

/// Why a batch has no vectors.
enum Halt {
    /// This batch failed; the model is still there for the next.
    Batch(String),
    /// The model is gone, stuck or not wanted: let go of it.
    Engine(String),
}

/// One `clew-speak embed`, with the model loaded.
struct Engine {
    child: Child,
    stdin: ChildStdin,
    answers: mpsc::Receiver<Result<Answer, String>>,
    program: PathBuf,
    dir: PathBuf,
    next: u64,
    used: Instant,
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Engine {
    /// Start `program` on the model in `dir`, and wait for it to have
    /// loaded it — unless that stops being wanted.
    fn start(program: &Path, dir: &Path, cancelled: &dyn Fn() -> bool) -> Result<Engine, String> {
        let mut child = crate::spawn::command(program)
            .arg("embed")
            .arg(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start the embedding model: {e}"))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("the embedding model could not be spoken to".into());
        };
        let (tx, answers) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("clew-embed-answers".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let answer = match line {
                        Ok(line) => serde_json::from_str::<Answer>(&line).map_err(|e| {
                            format!("the embedding model said something unreadable: {e}")
                        }),
                        Err(e) => Err(format!("the embedding model could not be heard: {e}")),
                    };
                    let stop = answer.is_err();
                    if tx.send(answer).is_err() || stop {
                        break;
                    }
                }
            });
        let mut engine = Engine {
            child,
            stdin,
            answers,
            program: program.to_path_buf(),
            dir: dir.to_path_buf(),
            next: 0,
            used: Instant::now(),
        };
        reader.map_err(|e| format!("could not start the embedding model: {e}"))?;
        let started = Instant::now();
        loop {
            if cancelled() {
                return Err(crate::llm::CANCELLED.into());
            }
            match engine.answers.recv_timeout(POLL) {
                Ok(Ok(Answer::Ready { dims })) if dims > 0 => {
                    engine.used = Instant::now();
                    return Ok(engine);
                }
                Ok(Ok(Answer::Error(e))) | Ok(Err(e)) => return Err(e),
                Ok(Ok(_)) => {
                    return Err("the embedding model answered before it was ready".into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) if started.elapsed() < READY_TIMEOUT => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err("the embedding model took too long to load".into());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(
                        "the embedding model ended as it started — its files may be damaged".into(),
                    );
                }
            }
        }
    }

    /// The vectors of `texts`, in their order.
    fn embed(
        &mut self,
        texts: &[String],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<Vec<f32>>, Halt> {
        self.next += 1;
        let id = self.next;
        let line = serde_json::json!({ "embed": { "id": id, "texts": texts } });
        writeln!(self.stdin, "{line}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| Halt::Engine(format!("the embedding model could not be asked: {e}")))?;
        let started = Instant::now();
        loop {
            if cancelled() {
                return Err(Halt::Engine(crate::llm::CANCELLED.into()));
            }
            match self.answers.recv_timeout(POLL) {
                Ok(Ok(Answer::Vectors { id: of, dims, data })) if of == id => {
                    return decode(&data, dims, texts.len()).map_err(Halt::Engine);
                }
                Ok(Ok(Answer::Failed { id: of, error })) if of == id => {
                    return Err(Halt::Batch(error));
                }
                Ok(Ok(Answer::Error(e))) | Ok(Err(e)) => return Err(Halt::Engine(e)),
                // An answer to no batch asked now: not this one's.
                Ok(Ok(_)) => {}
                Err(mpsc::RecvTimeoutError::Timeout) if started.elapsed() < BATCH_TIMEOUT => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(Halt::Engine("the embedding model stopped answering".into()));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Halt::Engine(
                        "the embedding model ended unexpectedly".into(),
                    ));
                }
            }
        }
    }
}

/// `count` vectors of `dims` from their little-endian bytes in base64 —
/// all of them, each finite, or none.
fn decode(data: &str, dims: usize, count: usize) -> Result<Vec<Vec<f32>>, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| "the embedding model's vectors could not be read".to_string())?;
    if dims == 0 || bytes.len() != count * dims * 4 {
        return Err("the embedding model answered with the wrong number of vectors".into());
    }
    let vectors: Vec<Vec<f32>> = bytes
        .chunks_exact(dims * 4)
        .map(|row| {
            row.chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect()
        })
        .collect();
    if vectors.iter().flatten().any(|x| !x.is_finite()) {
        return Err("the embedding model made a vector that is not a number".into());
    }
    Ok(vectors)
}

/// The model, while it is loaded: one at a time, kept between batches.
pub(crate) struct Engines {
    engine: Mutex<Option<Engine>>,
}

/// The engine of this process.
static ENGINES: Engines = Engines::new();

impl Engines {
    pub(crate) const fn new() -> Engines {
        Engines {
            engine: Mutex::new(None),
        }
    }

    /// The engine's slot, once no other batch holds it — or `None` once
    /// waiting stops being wanted.
    fn slot(&self, cancelled: &dyn Fn() -> bool) -> Option<MutexGuard<'_, Option<Engine>>> {
        loop {
            match self.engine.try_lock() {
                Ok(guard) => return Some(guard),
                Err(TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => {
                    if cancelled() {
                        return None;
                    }
                    std::thread::sleep(POLL);
                }
            }
        }
    }

    /// The vectors of `texts`, each with the prompt for `purpose`, by the
    /// model in `dir` that `program` runs — started when it is not running.
    pub(crate) fn embed(
        &self,
        program: &Path,
        dir: &Path,
        texts: &[String],
        purpose: Purpose,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<Vec<f32>>, String> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(BATCH) {
            let prompted: Vec<String> = chunk.iter().map(|t| prompted(purpose, t)).collect();
            out.extend(self.batch(program, dir, &prompted, cancelled)?);
        }
        Ok(out)
    }

    fn batch(
        &self,
        program: &Path,
        dir: &Path,
        texts: &[String],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<Vec<f32>>, String> {
        if cancelled() {
            return Err(crate::llm::CANCELLED.into());
        }
        let Some(mut slot) = self.slot(cancelled) else {
            return Err(crate::llm::CANCELLED.into());
        };
        // Another program or model than the one loaded: that one goes.
        if slot
            .as_ref()
            .is_some_and(|e| e.program != program || e.dir != dir)
        {
            *slot = None;
        }
        if slot.is_none() {
            *slot = Some(Engine::start(program, dir, cancelled)?);
        }
        let engine = slot.as_mut().expect("started above");
        let result = engine.embed(texts, cancelled);
        engine.used = Instant::now();
        match result {
            Ok(vectors) => Ok(vectors),
            Err(Halt::Batch(e)) => Err(e),
            Err(Halt::Engine(e)) => {
                *slot = None;
                Err(e)
            }
        }
    }

    /// Let the model go if nothing was asked of it for `after`; whether it
    /// was. One that is busy is not idle.
    pub(crate) fn unload_if_idle(&self, after: Duration) -> bool {
        let Ok(mut slot) = self.engine.try_lock() else {
            return false;
        };
        if slot.as_ref().is_some_and(|e| e.used.elapsed() >= after) {
            *slot = None;
            return true;
        }
        false
    }

    /// Let the model go now, waiting for a batch it is making: its files
    /// are about to be removed.
    pub(crate) fn unload(&self) {
        let mut slot = match self.engine.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = None;
    }

    /// Whether the model is loaded.
    #[cfg(test)]
    pub(crate) fn loaded(&self) -> bool {
        self.engine.lock().map(|s| s.is_some()).unwrap_or(false)
    }
}

/// Let go of the model once it has been quiet for [`UNLOAD_AFTER`]: a
/// thread a process, started with its first embedding.
fn watch_idle() {
    static WATCH: std::sync::Once = std::sync::Once::new();
    WATCH.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("clew-embed-idle".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(15));
                    ENGINES.unload_if_idle(UNLOAD_AFTER);
                }
            });
    });
}

/// The vectors of `texts`, made on this computer for `purpose`. Blocking —
/// run off the UI thread — and given up the moment `cancelled` says so,
/// with the batch being made: the model is stopped.
pub fn embed(
    texts: &[String],
    purpose: Purpose,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let program = program().ok_or_else(|| {
        "clew-speak, which makes embeddings on this computer, is not installed beside Clew"
            .to_string()
    })?;
    let Some(dir) = dir().filter(|dir| present(dir)) else {
        // Removed since it was loaded: what is still in memory goes too.
        ENGINES.unload();
        return Err("EmbeddingGemma is not downloaded: get it in Settings → Embeddings".into());
    };
    watch_idle();
    ENGINES.embed(&program, &dir, texts, purpose, cancelled)
}

/// Let go of the model now (its files are being removed), waiting for a
/// batch it is making.
pub fn unload() {
    ENGINES.unload();
}

/// Remove the model's files, and whatever a download of it left — the
/// model let go of first.
pub fn remove() -> std::io::Result<()> {
    unload();
    let Some(dir) = dir() else {
        return Ok(());
    };
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Every file is pinned: a revision, a size, a SHA-256, inside the
    /// model's folder.
    #[test]
    fn the_model_is_pinned() {
        assert_eq!(MODEL.revision.len(), 40);
        assert!(MODEL.revision.bytes().all(|b| b.is_ascii_hexdigit()));
        for file in MODEL.files {
            assert_eq!(file.sha256.len(), 64, "{}", file.path);
            assert!(
                file.sha256
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            );
            assert!(file.bytes > 0);
            assert!(!file.path.starts_with('/') && !file.path.contains(".."));
            assert!(MODEL.url(file).starts_with("https://huggingface.co/"));
        }
        assert_eq!(MODEL.size_mb(), 217);
    }

    /// Each text is read with the prompt for what it is for.
    #[test]
    fn texts_carry_the_prompt_for_their_purpose() {
        assert_eq!(
            prompted(Purpose::Document, "Sums the cart."),
            "title: none | text: Sums the cart."
        );
        assert_eq!(
            prompted(Purpose::Query, "where is the total?"),
            "task: search result | query: where is the total?"
        );
    }

    /// Vectors are read from their bytes — all asked for, each a number —
    /// or not at all.
    #[test]
    fn vectors_are_read_whole_or_not_at_all() {
        let two = "AACAPwAAAAAAAAAAAACAPwAAAAAAAAAA";
        assert_eq!(
            decode(two, 3, 2).unwrap(),
            [vec![1.0, 0.0, 0.0], vec![1.0, 0.0, 0.0]]
        );
        assert!(decode(two, 3, 3).is_err(), "one short");
        assert!(decode(two, 0, 0).is_err());
        assert!(decode("not base64!", 3, 1).is_err());
        // NaN, 1, 0.
        let nan = base64::engine::general_purpose::STANDARD.encode(
            [f32::NAN, 1.0, 0.0]
                .iter()
                .flat_map(|x| x.to_le_bytes())
                .collect::<Vec<u8>>(),
        );
        assert!(decode(&nan, 3, 1).is_err());
    }

    /// Whether the model is here: every file of it, as files.
    #[test]
    fn the_model_is_here_when_every_file_is() {
        let dir = crate::testutil::TempDir::new("embed-local");
        assert!(!present(dir.path()));
        for file in MODEL.files {
            let path = dir.path().join(file.path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
        }
        assert!(present(dir.path()));
        std::fs::remove_file(dir.path().join(MODEL.files[0].path)).unwrap();
        std::fs::create_dir(dir.path().join(MODEL.files[0].path)).unwrap();
        assert!(!present(dir.path()), "a folder in a file's place");
    }

    /// A stand-in for `clew-speak embed`, as a shell script: it says it is
    /// ready (vectors of 3), then runs `body` for each request line in
    /// `$line`, its number in `$n`. `vectors` prints one vector a text.
    #[cfg(unix)]
    fn engine(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let script = format!(
            "[ \"$1\" = embed ] || exit 9\n\
             vectors() {{\n\
               count=$(printf '%s' \"$line\" | grep -o '\",\"' | wc -l)\n\
               count=$((count + 1)); data=''; i=0\n\
               while [ $i -lt $count ]; do data=\"${{data}}AACAPwAAAAAAAAAA\"; i=$((i + 1)); done\n\
               echo \"{{\\\"vectors\\\":{{\\\"id\\\":$n,\\\"dims\\\":3,\\\"data\\\":\\\"$data\\\"}}}}\"\n\
             }}\n\
             echo '{{\"ready\":{{\"dims\":3}}}}'\n\
             n=0\n\
             while read -r line; do\n\
               n=$((n + 1))\n\
               {body}\n\
             done"
        );
        crate::testutil::write_script(&path, &script);
        path
    }

    fn texts(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("text {i}")).collect()
    }

    /// Texts are embedded in batches, in their order, each with its
    /// prompt — and the model is kept for the next call.
    #[cfg(unix)]
    #[test]
    fn texts_are_embedded_in_batches_by_one_engine() {
        let dir = crate::testutil::TempDir::new("embed-local");
        let log = dir.path().join("asked");
        let program = engine(
            dir.path(),
            "engine",
            &format!("printf '%s\\n' \"$line\" >> '{}'; vectors", log.display()),
        );
        let engines = Engines::new();
        let vectors = engines
            .embed(&program, dir.path(), &texts(20), Purpose::Query, &|| false)
            .unwrap();
        assert_eq!(vectors.len(), 20);
        assert!(vectors.iter().all(|v| v == &[1.0, 0.0, 0.0]));
        assert!(engines.loaded(), "kept for the next");
        let asked = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = asked.lines().collect();
        assert_eq!(lines.len(), 2, "16, then 4");
        assert!(lines[0].contains("\"id\":1") && lines[1].contains("\"id\":2"));
        assert!(lines[0].contains("task: search result | query: text 0"));
        assert!(lines[1].contains("query: text 19"));
        // The same engine answers again: its third request.
        engines
            .embed(&program, dir.path(), &texts(1), Purpose::Document, &|| {
                false
            })
            .unwrap();
        let asked = std::fs::read_to_string(&log).unwrap();
        assert!(asked.lines().nth(2).unwrap().contains("\"id\":3"));
        assert!(asked.contains("title: none | text: text 0"));
        // Quiet long enough, it is let go.
        assert!(!engines.unload_if_idle(Duration::from_secs(600)));
        assert!(engines.unload_if_idle(Duration::ZERO));
        assert!(!engines.loaded());
    }

    /// A model that cannot load says why, and nothing is kept.
    #[cfg(unix)]
    #[test]
    fn a_model_that_cannot_load_says_why() {
        let dir = crate::testutil::TempDir::new("embed-local");
        let program = dir.path().join("broken");
        crate::testutil::write_script(
            &program,
            "echo '{\"error\":\"the model could not be loaded: no file\"}'\nexit 1",
        );
        let engines = Engines::new();
        let err = engines
            .embed(&program, dir.path(), &texts(1), Purpose::Query, &|| false)
            .unwrap_err();
        assert_eq!(err, "the model could not be loaded: no file");
        assert!(!engines.loaded());
        let missing = dir.path().join("not-there");
        let err = engines
            .embed(&missing, dir.path(), &texts(1), Purpose::Query, &|| false)
            .unwrap_err();
        assert!(
            err.starts_with("could not start the embedding model"),
            "{err}"
        );
    }

    /// A batch the model could not make fails alone: the model stays, and
    /// makes the next.
    #[cfg(unix)]
    #[test]
    fn a_failed_batch_keeps_the_model() {
        let dir = crate::testutil::TempDir::new("embed-local");
        let program = engine(
            dir.path(),
            "engine",
            "if [ $n = 1 ]; then echo '{\"failed\":{\"id\":1,\"error\":\"a text could not be read\"}}'; else vectors; fi",
        );
        let engines = Engines::new();
        let err = engines
            .embed(&program, dir.path(), &texts(2), Purpose::Query, &|| false)
            .unwrap_err();
        assert_eq!(err, "a text could not be read");
        assert!(engines.loaded());
        assert_eq!(
            engines
                .embed(&program, dir.path(), &texts(2), Purpose::Query, &|| false)
                .unwrap()
                .len(),
            2
        );
    }

    /// A model that ends, or answers wrong, is let go — and started again
    /// for the next call.
    #[cfg(unix)]
    #[test]
    fn a_model_that_ends_is_started_again() {
        let dir = crate::testutil::TempDir::new("embed-local");
        let engines = Engines::new();
        let ask = |program: &Path| {
            engines.embed(program, dir.path(), &texts(1), Purpose::Query, &|| false)
        };
        let ends = engine(
            dir.path(),
            "ends",
            "if [ $n = 2 ]; then exit 0; fi; vectors",
        );
        assert!(ask(&ends).is_ok());
        assert_eq!(
            ask(&ends).unwrap_err(),
            "the embedding model ended unexpectedly"
        );
        assert!(!engines.loaded());
        assert!(ask(&ends).is_ok(), "a new one");
        let short = engine(
            dir.path(),
            "short",
            "if [ $n = 2 ]; then echo '{\"vectors\":{\"id\":2,\"dims\":3,\"data\":\"\"}}'; else vectors; fi",
        );
        assert!(ask(&short).is_ok());
        assert_eq!(
            ask(&short).unwrap_err(),
            "the embedding model answered with the wrong number of vectors"
        );
        assert!(!engines.loaded(), "let go");
        assert!(ask(&short).is_ok());
    }

    /// A stop is heard at once, while the model works on a batch: the
    /// model is stopped with it, and the next call starts another.
    #[cfg(unix)]
    #[test]
    fn a_stop_ends_the_batch_and_the_model() {
        let dir = crate::testutil::TempDir::new("embed-local");
        let first = dir.path().join("first");
        // The first engine never answers; the next does.
        let program = engine(
            dir.path(),
            "engine",
            &format!(
                "if [ ! -e '{0}' ]; then : > '{0}'; exec sleep 600; fi; vectors",
                first.display()
            ),
        );
        let engines = Engines::new();
        let stop = Arc::new(AtomicBool::new(false));
        let stopper = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                stop.store(true, Ordering::Relaxed);
            })
        };
        let started = Instant::now();
        let err = engines
            .embed(&program, dir.path(), &texts(1), Purpose::Query, &|| {
                stop.load(Ordering::Relaxed)
            })
            .unwrap_err();
        stopper.join().unwrap();
        assert_eq!(err, crate::llm::CANCELLED);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!engines.loaded(), "stopped with it");
        assert_eq!(
            engines
                .embed(&program, dir.path(), &texts(1), Purpose::Query, &|| false)
                .unwrap()
                .len(),
            1
        );
    }

    /// Another model folder, or another program, than the one loaded: that
    /// one is let go and the asked one started.
    #[cfg(unix)]
    #[test]
    fn another_model_replaces_the_one_loaded() {
        let dir = crate::testutil::TempDir::new("embed-local");
        let log = dir.path().join("started");
        let program = engine(dir.path(), "engine", "vectors");
        // Wrapped, to count the starts.
        let counted = dir.path().join("counted");
        crate::testutil::write_script(
            &counted,
            &format!(
                "echo start >> '{}'\nexec '{}' \"$@\"",
                log.display(),
                program.display()
            ),
        );
        let other = crate::testutil::TempDir::new("embed-local");
        let engines = Engines::new();
        let ask = |dir: &Path| {
            engines
                .embed(&counted, dir, &texts(1), Purpose::Query, &|| false)
                .unwrap()
        };
        ask(dir.path());
        ask(dir.path());
        ask(other.path());
        let starts = std::fs::read_to_string(&log).unwrap().lines().count();
        assert_eq!(starts, 2, "once a model folder");
    }

    /// EmbeddingGemma itself, when the test is pointed at `clew-speak` and
    /// the model: a query is nearer the text it asks about than to another,
    /// as each is told what it is for.
    #[test]
    #[ignore = "needs CLEW_SPEAK and CLEW_EMBED_MODEL"]
    fn the_real_model_finds_what_is_asked_for() {
        let program = PathBuf::from(std::env::var("CLEW_SPEAK").unwrap());
        let dir = PathBuf::from(std::env::var("CLEW_EMBED_MODEL").unwrap());
        let engines = Engines::new();
        let docs = engines
            .embed(
                &program,
                &dir,
                &[
                    "Sums the price of every item in the cart.".to_string(),
                    "Opens a connection to the database.".to_string(),
                ],
                Purpose::Document,
                &|| false,
            )
            .unwrap();
        let query = engines
            .embed(
                &program,
                &dir,
                &["where is the cart total computed?".to_string()],
                Purpose::Query,
                &|| false,
            )
            .unwrap();
        assert_eq!(docs[0].len(), 768);
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!(dot(&query[0], &docs[0]) > dot(&query[0], &docs[1]) + 0.1);
    }
}
