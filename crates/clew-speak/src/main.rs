//! `clew-speak`: text read aloud, on this computer, by one of the voice
//! models Clew offers (`clew_voice::speech`).
//!
//! It is a process of its own because the speech runtime — ONNX Runtime,
//! inside sherpa-onnx — cannot share one with whisper.cpp: on Linux its
//! prebuilt library is built against the older C++ string ABI, and the two
//! copies of the C++ standard library's templates the linker merges then
//! crash it as it starts. Apart, neither can take the other — or the
//! editor — down, and what is being said stops the moment the process is
//! told to, or ends.
//!
//! It loads the model it is given once, then reads requests from stdin and
//! writes answers to stdout, one JSON object per line:
//!
//! ```text
//! clew-speak supertonic <model dir>     (or: kokoro <model dir>)
//! → {"ready":{"rate":44100,"voices":10}}
//! ← {"say":{"id":1,"text":"Hello.","lang":"en","voice":0,"speed":1.0}}
//! → {"audio":{"id":1,"pcm":"<base64: 16-bit little-endian samples>","more":true}}
//! → {"audio":{"id":1,"pcm":""}}
//! ```
//!
//! A sentence is answered in parts, as the model makes them — a clause of
//! Chinese at a time, so its first words play while the rest is still
//! being made — each `"more":true`, then the last with `"more"` left out
//! (its `pcm` may be empty).
//!
//! It also makes embeddings (`clew-speak embed <model dir>`, [`embed`]),
//! hears when a speaker has finished (`clew-speak turn <model file>`,
//! [`turn`]), and waits to be called (`clew-speak listen <model dir>
//! <keyword>…`, [`listen`]).
//!
//! A request that cannot be met is answered `{"failed":{"id":1,"error":…}}`;
//! a model that cannot be loaded, `{"error":…}` before it exits. It exits
//! when stdin closes.
//!
//! What is no longer wanted — the reader stopped it — is said
//! `{"cancel":{"id":1}}`: the sentence being made stops at its next part
//! (answered `failed`), and one not yet begun is passed over. The next
//! sentence waits for none of it.

mod embed;
mod listen;
mod mel;
mod runtime;
mod turn;

use std::cell::{Cell, RefCell};
use std::io::{BufRead, Write};
use std::path::Path;
use std::rc::Rc;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// A model family, and the files of its folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// Supertonic: characters in, any of its languages.
    Supertonic,
    /// Kokoro v1.1-zh: Chinese, the English words in it too, read through
    /// its lexicons; a clause at a time.
    Kokoro,
}

impl Family {
    fn parse(name: &str) -> Option<Family> {
        match name {
            "supertonic" => Some(Family::Supertonic),
            "kokoro" => Some(Family::Kokoro),
            _ => None,
        }
    }
}

/// One request: say `text`.
#[derive(Debug, Deserialize)]
struct Say {
    id: u64,
    text: String,
    /// Its language, as a code (`en`, `ko`, `fr`): how it is pronounced.
    #[serde(default)]
    lang: Option<String>,
    /// The model's voice, by number.
    #[serde(default)]
    voice: i32,
    /// 1 is the model's own pace; more is faster.
    #[serde(default = "one")]
    speed: f32,
}

fn one() -> f32 {
    1.0
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Request {
    Say(Say),
    /// The sentence `id` is no longer wanted.
    Cancel {
        id: u64,
    },
}

/// What stdin says, read on a thread of its own so a sentence being made
/// hears a `cancel` for it: requests waiting their turn, in order.
struct Inbox {
    lines: std::sync::mpsc::Receiver<String>,
    /// Requests read while a sentence was being made, not yet answered.
    waiting: std::collections::VecDeque<String>,
    /// Sentences cancelled before they were begun.
    cancelled: std::collections::HashSet<u64>,
}

impl Inbox {
    fn new(lines: std::sync::mpsc::Receiver<String>) -> Inbox {
        Inbox {
            lines,
            waiting: std::collections::VecDeque::new(),
            cancelled: std::collections::HashSet::new(),
        }
    }

    /// The next request's line: one read while a sentence was being made,
    /// else the next to come. `None` once stdin has closed.
    fn next(&mut self) -> Option<String> {
        self.waiting.pop_front().or_else(|| self.lines.recv().ok())
    }

    /// Whether the sentence `id`, being made, has been cancelled: what has
    /// come meanwhile is read, a cancel kept, anything else kept for its
    /// turn.
    fn cancelled(&mut self, id: u64) -> bool {
        while let Ok(line) = self.lines.try_recv() {
            match serde_json::from_str::<Request>(&line) {
                Ok(Request::Cancel { id: gone }) => {
                    self.cancelled.insert(gone);
                }
                _ => self.waiting.push_back(line),
            }
        }
        self.cancelled.contains(&id)
    }

    /// Whether the sentence `id`, about to be begun, was cancelled already;
    /// forgotten after this.
    fn take_cancelled(&mut self, id: u64) -> bool {
        self.cancelled(id);
        self.cancelled.remove(&id)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Answer {
    Ready {
        rate: i32,
        voices: i32,
    },
    Audio {
        id: u64,
        pcm: String,
        /// More of the sentence is coming.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        more: bool,
    },
    Failed {
        id: u64,
        error: String,
    },
    Error(String),
}

fn main() {
    // As the system gives them: a folder's name need not be Unicode (on
    // Linux, any bytes), and `args` would end the program on one that is not.
    let mut args = std::env::args_os().skip(1);
    let first = args.next().map(|a| a.to_string_lossy().into_owned());
    if first.as_deref() == Some("--version") {
        println!("clew-speak {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if first.as_deref() == Some("embed") {
        let Some(dir) = args.next() else {
            eprintln!("usage: clew-speak embed <model dir>");
            std::process::exit(2);
        };
        std::process::exit(embed::run(Path::new(&dir)));
    }
    if first.as_deref() == Some("listen") {
        let Some(dir) = args.next() else {
            eprintln!("usage: clew-speak listen <model dir> <keyword>…");
            std::process::exit(2);
        };
        let keywords: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
        std::process::exit(listen::run(Path::new(&dir), &keywords));
    }
    if first.as_deref() == Some("turn") {
        let Some(file) = args.next() else {
            eprintln!("usage: clew-speak turn <model file>");
            std::process::exit(2);
        };
        std::process::exit(turn::run(Path::new(&file)));
    }
    let (Some(family), Some(dir)) = (first.as_deref().and_then(Family::parse), args.next()) else {
        eprintln!("usage: clew-speak supertonic|kokoro <model dir>");
        std::process::exit(2);
    };
    let tts = match load(family, Path::new(&dir)) {
        Ok(tts) => tts,
        Err(e) => {
            let _ = send(&Answer::Error(e));
            std::process::exit(1);
        }
    };
    let ready = Answer::Ready {
        rate: tts.sample_rate(),
        voices: tts.num_speakers().max(1),
    };
    if send(&ready).is_err() {
        return;
    }
    let (sender, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    // Shared with the parts' callback, which hears a cancel between them.
    let inbox = Rc::new(RefCell::new(Inbox::new(lines)));
    loop {
        let Some(line) = inbox.borrow_mut().next() else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let say = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Say(say)) => say,
            // For a sentence already made, or not yet begun.
            Ok(Request::Cancel { id }) => {
                inbox.borrow_mut().cancelled.insert(id);
                continue;
            }
            Err(e) => {
                if send(&Answer::Error(format!("not a request: {e}"))).is_err() {
                    break;
                }
                continue;
            }
        };
        let id = say.id;
        if inbox.borrow_mut().take_cancelled(id) {
            let answer = Answer::Failed {
                id,
                error: "cancelled".into(),
            };
            if send(&answer).is_err() {
                break;
            }
            continue;
        }
        // Each part as it is made; a reader gone ends it all.
        let gone = Rc::new(Cell::new(false));
        let part = {
            let (inbox, gone) = (Rc::clone(&inbox), Rc::clone(&gone));
            move |samples: &[f32]| {
                if inbox.borrow_mut().cancelled(id) {
                    return false;
                }
                let part = Answer::Audio {
                    id,
                    pcm: pcm16(samples),
                    more: true,
                };
                let sent = send(&part).is_ok();
                gone.set(!sent);
                sent
            }
        };
        let said = speak(&tts, family, &say, part);
        if gone.get() {
            break;
        }
        let cancelled = inbox.borrow_mut().cancelled.remove(&id);
        let answer = match said {
            _ if cancelled => Answer::Failed {
                id,
                error: "cancelled".into(),
            },
            Ok(rest) => Answer::Audio {
                id,
                pcm: if rest.is_empty() {
                    String::new()
                } else {
                    pcm16(&rest)
                },
                more: false,
            },
            Err(error) => Answer::Failed { id, error },
        };
        if send(&answer).is_err() {
            break;
        }
    }
}

/// Write one answer to stdout as a line, flushed: the reader waits for it.
fn send(answer: &Answer) -> std::io::Result<()> {
    let line = serde_json::to_string(answer).map_err(std::io::Error::other)?;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")?;
    out.flush()
}

/// The model in `dir`, by the files its family's folder holds.
fn load(family: Family, dir: &Path) -> Result<sherpa_onnx::OfflineTts, String> {
    use sherpa_onnx::*;
    // Whole, as Kokoro's lists are named from inside it.
    let dir = &std::path::absolute(dir).map_err(|e| format!("no model folder: {e}"))?;
    let file = |name: &str| -> Result<String, String> {
        let path = dir.join(name);
        if path.is_file() {
            Ok(path.to_string_lossy().into_owned())
        } else {
            Err(format!("the model has no {name}"))
        }
    };
    let mut config = OfflineTtsConfig::default();
    match family {
        Family::Supertonic => {
            config.model.supertonic = OfflineTtsSupertonicModelConfig {
                duration_predictor: Some(file("duration_predictor.int8.onnx")?),
                text_encoder: Some(file("text_encoder.int8.onnx")?),
                vector_estimator: Some(file("vector_estimator.int8.onnx")?),
                vocoder: Some(file("vocoder.int8.onnx")?),
                tts_json: Some(file("tts.json")?),
                unicode_indexer: Some(file("unicode_indexer.bin")?),
                voice_style: Some(file("voice.bin")?),
            };
        }
        Family::Kokoro => {
            // Its lexicons and rules are lists, split at commas: named from
            // the model's folder, so a comma in the path to it splits none.
            std::env::set_current_dir(dir)
                .map_err(|e| format!("the model's folder cannot be read: {e}"))?;
            let here =
                |name: &str| -> Result<String, String> { file(name).map(|_| name.to_string()) };
            if !dir.join("dict").is_dir() {
                return Err("the model has no dict".into());
            }
            config.model.kokoro = OfflineTtsKokoroModelConfig {
                model: Some(file("model.onnx")?),
                voices: Some(file("voices.bin")?),
                tokens: Some(file("tokens.txt")?),
                lexicon: Some([here("lexicon-us-en.txt")?, here("lexicon-zh.txt")?].join(",")),
                dict_dir: Some("dict".into()),
                ..Default::default()
            };
            config.rule_fsts = Some(
                [
                    here("date-zh.fst")?,
                    here("number-zh.fst")?,
                    here("phone-zh.fst")?,
                ]
                .join(","),
            );
        }
    }
    // A sentence at a time — for Chinese, a clause — each given as it is
    // made ([`speak`]).
    config.max_num_sentences = 1;
    // Half the cores, at least one: the editor goes on drawing meanwhile.
    config.model.num_threads =
        std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(1)) as i32;
    match OfflineTts::create(&config) {
        Some(tts) => Ok(tts),
        None => Err("the model could not be loaded".into()),
    }
}

/// How many steps Supertonic refines each sentence's sound in. Its own
/// default, 5, keeps a grain in the voice that 8 smooths out; each step
/// costs about a fortieth of the sentence's length on two cores of a slow
/// laptop CPU, so 8 still makes speech some four times faster than it is
/// said, well ahead of the reader.
const SUPERTONIC_STEPS: i32 = 8;

/// How `say` is made with `family`'s model, of `speakers` voices.
fn generation(family: Family, say: &Say, speakers: i32) -> sherpa_onnx::GenerationConfig {
    let mut config = sherpa_onnx::GenerationConfig {
        sid: say.voice.clamp(0, (speakers - 1).max(0)),
        speed: say.speed.clamp(0.5, 2.0),
        ..Default::default()
    };
    if family == Family::Supertonic {
        config.num_steps = SUPERTONIC_STEPS;
        if let Some(lang) = &say.lang {
            config.extra = Some(
                [("lang".to_string(), serde_json::Value::from(lang.as_str()))]
                    .into_iter()
                    .collect(),
            );
        }
    }
    config
}

/// `say`'s text spoken, at the model's rate: each part — a sentence of
/// it, a clause of Chinese — given to `part` as it is made, which answers
/// whether more is wanted; then whatever of it was not given that way.
fn speak(
    tts: &sherpa_onnx::OfflineTts,
    family: Family,
    say: &Say,
    mut part: impl FnMut(&[f32]) -> bool + 'static,
) -> Result<Vec<f32>, String> {
    let config = generation(family, say, tts.num_speakers());
    let given = Rc::new(Cell::new(0));
    let counted = Rc::clone(&given);
    let callback = move |samples: &[f32], _progress: f32| {
        let more = part(samples);
        if more {
            counted.set(counted.get() + samples.len());
        }
        more
    };
    let Some(audio) = tts.generate_with_config(&say.text, &config, Some(callback)) else {
        return Err("no audio could be made from the text".into());
    };
    Ok(audio
        .samples()
        .get(given.get()..)
        .unwrap_or_default()
        .to_vec())
}

/// Samples in [-1, 1] as 16-bit little-endian PCM, in base64.
fn pcm16(samples: &[f32]) -> String {
    let bytes: Vec<u8> = samples
        .iter()
        .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    /// Supertonic refines each sentence in more steps than its default —
    /// a smoother voice — in the language it is told.
    #[test]
    fn supertonic_speaks_in_more_steps_than_its_default() {
        let say = |lang: Option<&str>| super::Say {
            id: 1,
            text: "Hello.".into(),
            lang: lang.map(String::from),
            voice: 9,
            speed: 1.0,
        };
        let config = super::generation(super::Family::Supertonic, &say(Some("fr")), 10);
        assert_eq!(config.num_steps, super::SUPERTONIC_STEPS);
        assert!(super::SUPERTONIC_STEPS > sherpa_onnx::GenerationConfig::default().num_steps);
        assert_eq!(config.sid, 9);
        assert_eq!(
            config.extra.as_ref().and_then(|e| e.get("lang")),
            Some(&serde_json::Value::from("fr"))
        );
        let config = super::generation(super::Family::Supertonic, &say(None), 4);
        assert_eq!(config.sid, 3, "a voice it has");
        assert!(config.extra.is_none());
    }

    use super::*;

    /// While a sentence is made, a cancel for it is heard and kept; any
    /// other request read meanwhile waits its turn, in order; and a sentence
    /// cancelled before it began is passed over once.
    #[test]
    fn the_inbox_hears_a_cancel_and_keeps_the_rest_in_order() {
        let (send, lines) = std::sync::mpsc::channel();
        let mut inbox = Inbox::new(lines);
        send.send(r#"{"say":{"id":2,"text":"Next."}}"#.to_string())
            .unwrap();
        assert!(!inbox.cancelled(1));
        send.send(r#"{"cancel":{"id":1}}"#.to_string()).unwrap();
        send.send(r#"{"cancel":{"id":3}}"#.to_string()).unwrap();
        send.send(r#"{"say":{"id":3,"text":"Gone."}}"#.to_string())
            .unwrap();
        assert!(inbox.cancelled(1), "the sentence being made");
        assert!(inbox.next().unwrap().contains(r#""id":2"#), "in its turn");
        assert!(inbox.next().unwrap().contains(r#""id":3"#));
        assert!(inbox.take_cancelled(3), "cancelled before it began");
        assert!(!inbox.take_cancelled(3), "once");
        drop(send);
        assert!(inbox.next().is_none(), "stdin closed");
        let Request::Cancel { id } = serde_json::from_str(r#"{"cancel":{"id":7}}"#).unwrap() else {
            panic!("a cancel");
        };
        assert_eq!(id, 7);
    }

    /// A request reads as the client writes it; what is left out is the
    /// model's own.
    #[test]
    fn requests_and_answers_read_as_written() {
        let Ok(Request::Say(say)) =
            serde_json::from_str(r#"{"say":{"id":3,"text":"Hi.","lang":"ko"}}"#)
        else {
            panic!("a say");
        };
        assert_eq!((say.id, say.voice, say.speed), (3, 0, 1.0));
        assert_eq!(say.lang.as_deref(), Some("ko"));
        assert_eq!(
            serde_json::to_string(&Answer::Ready {
                rate: 44100,
                voices: 10
            })
            .unwrap(),
            r#"{"ready":{"rate":44100,"voices":10}}"#
        );
        assert_eq!(
            serde_json::to_string(&Answer::Error("x".into())).unwrap(),
            r#"{"error":"x"}"#
        );
        // A sentence whole reads as it always has; a part says more comes.
        let audio = |more| Answer::Audio {
            id: 1,
            pcm: "AA==".into(),
            more,
        };
        assert_eq!(
            serde_json::to_string(&audio(false)).unwrap(),
            r#"{"audio":{"id":1,"pcm":"AA=="}}"#
        );
        assert_eq!(
            serde_json::to_string(&audio(true)).unwrap(),
            r#"{"audio":{"id":1,"pcm":"AA==","more":true}}"#
        );
    }

    /// Samples become 16-bit PCM, clipped at full scale.
    #[test]
    fn samples_become_pcm() {
        let pcm = pcm16(&[0.0, 1.0, -2.0]);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(pcm)
            .unwrap();
        assert_eq!(bytes, [0, 0, 0xff, 0x7f, 0x01, 0x80]);
    }

    /// A family is named as Clew's catalog names it.
    #[test]
    fn families_are_named() {
        assert_eq!(Family::parse("supertonic"), Some(Family::Supertonic));
        assert_eq!(Family::parse("kokoro"), Some(Family::Kokoro));
        assert_eq!(Family::parse("qwen3"), None, "retired");
        assert_eq!(Family::parse("piper"), None);
    }
}
