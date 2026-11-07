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
//! → {"audio":{"id":1,"pcm":"<base64: 16-bit little-endian samples>"}}
//! ```
//!
//! A request that cannot be met is answered `{"failed":{"id":1,"error":…}}`;
//! a model that cannot be loaded, `{"error":…}` before it exits. It exits
//! when stdin closes.

use std::io::{BufRead, Write};
use std::path::Path;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// A model family, and the files of its folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// Supertonic: characters in, any of its 31 languages.
    Supertonic,
    /// Kokoro: words looked up in its lexicons (Mandarin and English).
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
    /// Its language, as a code (`en`, `ko`, `fr`): Supertonic's choice of
    /// pronunciation; Kokoro reads it from its lexicons.
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
    Ready { rate: i32, voices: i32 },
    Audio { id: u64, pcm: String },
    Failed { id: u64, error: String },
    Error(String),
}

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next();
    if first.as_deref() == Some("--version") {
        println!("clew-speak {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let (Some(family), Some(dir)) = (first.as_deref().and_then(Family::parse), args.next()) else {
        eprintln!("usage: clew-speak supertonic|kokoro <model dir>");
        std::process::exit(2);
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let tts = match load(family, Path::new(&dir)) {
        Ok(tts) => tts,
        Err(e) => {
            let _ = send(&mut out, &Answer::Error(e));
            std::process::exit(1);
        }
    };
    if send(
        &mut out,
        &Answer::Ready {
            rate: tts.sample_rate(),
            voices: tts.num_speakers().max(1),
        },
    )
    .is_err()
    {
        return;
    }
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Say(say)) => match speak(&tts, family, &say) {
                Ok(samples) => Answer::Audio {
                    id: say.id,
                    pcm: pcm16(&samples),
                },
                Err(error) => Answer::Failed { id: say.id, error },
            },
            Err(e) => Answer::Error(format!("not a request: {e}")),
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
        Family::Kokoro => {
            let lexicons = ["lexicon-us-en.txt", "lexicon-zh.txt"]
                .iter()
                .map(|name| file(name))
                .collect::<Result<Vec<_>, _>>()?;
            config.model.kokoro = OfflineTtsKokoroModelConfig {
                model: Some(file("model.int8.onnx")?),
                voices: Some(file("voices.bin")?),
                tokens: Some(file("tokens.txt")?),
                lexicon: Some(lexicons.join(",")),
                dict_dir: Some(dir.join("dict").to_string_lossy().into_owned()),
                ..Default::default()
            };
            config.rule_fsts = Some(
                ["date-zh.fst", "number-zh.fst", "phone-zh.fst"]
                    .iter()
                    .map(|name| file(name))
                    .collect::<Result<Vec<_>, _>>()?
                    .join(","),
            );
        }
    }
    // Half the cores, at least one: the editor goes on drawing meanwhile.
    config.model.num_threads =
        std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(1)) as i32;
    match OfflineTts::create(&config) {
        Some(tts) => Ok(tts),
        None => Err("the model could not be loaded".into()),
    }
}

/// `say`'s text as samples, at the model's rate.
fn speak(tts: &sherpa_onnx::OfflineTts, family: Family, say: &Say) -> Result<Vec<f32>, String> {
    let mut config = sherpa_onnx::GenerationConfig {
        sid: say.voice.clamp(0, (tts.num_speakers() - 1).max(0)),
        speed: say.speed.clamp(0.5, 2.0),
        ..Default::default()
    };
    if family == Family::Supertonic
        && let Some(lang) = &say.lang
    {
        config.extra = Some(
            [("lang".to_string(), serde_json::Value::from(lang.as_str()))]
                .into_iter()
                .collect(),
        );
    }
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
        assert_eq!(Family::parse("piper"), None);
    }
}
