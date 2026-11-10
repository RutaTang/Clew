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
//! clew-speak supertonic <model dir>     (or: qwen3 <model dir>)
//! → {"ready":{"rate":44100,"voices":10}}
//! ← {"say":{"id":1,"text":"Hello.","lang":"en","voice":0,"speed":1.0}}
//! → {"audio":{"id":1,"pcm":"<base64: 16-bit little-endian samples>"}}
//! ```
//!
//! A voice that streams ([`qwen3`]) answers a sentence in parts, as they
//! are made, each `"more":true` — then the last, `"more"` left out (its
//! `pcm` may be empty):
//!
//! ```text
//! → {"audio":{"id":1,"pcm":"…","more":true}}
//! → {"audio":{"id":1,"pcm":"…","more":true}}
//! → {"audio":{"id":1,"pcm":""}}
//! ```
//!
//! It also makes embeddings (`clew-speak embed <model dir>`, [`embed`]),
//! hears when a speaker has finished (`clew-speak turn <model file>`,
//! [`turn`]), and waits to be called (`clew-speak listen <model dir>
//! <keyword>…`, [`listen`]).
//!
//! A request that cannot be met is answered `{"failed":{"id":1,"error":…}}`;
//! a model that cannot be loaded, `{"error":…}` before it exits. It exits
//! when stdin closes.

mod embed;
mod graph;
mod listen;
mod mel;
mod qwen3;
mod runtime;
mod stretch;
mod turn;

use std::io::{BufRead, Write};
use std::path::Path;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// A model family, and the files of its folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// Supertonic: characters in, any of its languages; a sentence made
    /// whole.
    Supertonic,
    /// Qwen3-TTS: speech as a person says it, streamed ([`qwen3`]).
    Qwen3,
}

impl Family {
    fn parse(name: &str) -> Option<Family> {
        match name {
            "supertonic" => Some(Family::Supertonic),
            "qwen3" => Some(Family::Qwen3),
            _ => None,
        }
    }
}

/// A model loaded.
enum Voice {
    Sherpa(sherpa_onnx::OfflineTts),
    Qwen3(Box<qwen3::Qwen3>),
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
        eprintln!("usage: clew-speak supertonic|qwen3 <model dir>");
        std::process::exit(2);
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let loaded = match family {
        Family::Qwen3 => qwen3::Qwen3::load(Path::new(&dir)).map(|q| Voice::Qwen3(Box::new(q))),
        Family::Supertonic => load(family, Path::new(&dir)).map(Voice::Sherpa),
    };
    let mut voice = match loaded {
        Ok(voice) => voice,
        Err(e) => {
            let _ = send(&mut out, &Answer::Error(e));
            std::process::exit(1);
        }
    };
    let (rate, voices) = match &voice {
        Voice::Sherpa(tts) => (tts.sample_rate(), tts.num_speakers().max(1)),
        Voice::Qwen3(_) => (qwen3::RATE, qwen3::VOICES),
    };
    if send(&mut out, &Answer::Ready { rate, voices }).is_err() {
        return;
    }
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let say = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Say(say)) => say,
            Err(e) => {
                if send(&mut out, &Answer::Error(format!("not a request: {e}"))).is_err() {
                    break;
                }
                continue;
            }
        };
        let id = say.id;
        let answer = match &mut voice {
            Voice::Sherpa(tts) => match speak(tts, family, &say) {
                Ok(samples) => Answer::Audio {
                    id,
                    pcm: pcm16(&samples),
                    more: false,
                },
                Err(error) => Answer::Failed { id, error },
            },
            Voice::Qwen3(qwen3) => {
                // Each part as it is made; a reader gone ends it.
                let mut gone = false;
                let said = qwen3.say(
                    &say.text,
                    say.lang.as_deref().unwrap_or(""),
                    say.voice,
                    say.speed,
                    &mut |samples| {
                        let part = Answer::Audio {
                            id,
                            pcm: pcm16(samples),
                            more: true,
                        };
                        send(&mut out, &part).map_err(|e| {
                            gone = true;
                            e.to_string()
                        })
                    },
                );
                if gone {
                    break;
                }
                match said {
                    Ok(()) => Answer::Audio {
                        id,
                        pcm: String::new(),
                        more: false,
                    },
                    Err(error) => Answer::Failed { id, error },
                }
            }
        };
        if send(&mut out, &answer).is_err() {
            break;
        }
    }
}

/// Write one answer as a line, flushed: the reader waits for it.
fn send(out: &mut impl Write, answer: &Answer) -> std::io::Result<()> {
    let line = serde_json::to_string(answer).map_err(std::io::Error::other)?;
    writeln!(out, "{line}")?;
    out.flush()
}

/// The model in `dir`, by the files its family's folder holds.
fn load(family: Family, dir: &Path) -> Result<sherpa_onnx::OfflineTts, String> {
    use sherpa_onnx::*;
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
        Family::Qwen3 => return Err("not a sherpa-onnx model".into()),
    }
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

/// `say`'s text as samples, at the model's rate.
fn speak(tts: &sherpa_onnx::OfflineTts, family: Family, say: &Say) -> Result<Vec<f32>, String> {
    let config = generation(family, say, tts.num_speakers());
    let Some(audio) = tts.generate_with_config(&say.text, &config, None::<fn(&[f32], f32) -> bool>)
    else {
        return Err("nothing could be made of it".into());
    };
    Ok(audio.samples().to_vec())
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

    /// A request reads as the client writes it; what is left out is the
    /// model's own.
    #[test]
    fn requests_and_answers_read_as_written() {
        let Request::Say(say) =
            serde_json::from_str(r#"{"say":{"id":3,"text":"Hi.","lang":"ko"}}"#).unwrap();
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
        assert_eq!(Family::parse("qwen3"), Some(Family::Qwen3));
        assert_eq!(Family::parse("kokoro"), None);
        assert_eq!(Family::parse("piper"), None);
    }
}
