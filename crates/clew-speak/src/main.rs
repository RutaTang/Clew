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
//! It also makes embeddings (`clew-speak embed <model dir>`, [`embed`]),
//! hears when a speaker has finished (`clew-speak turn <model file>`,
//! [`turn`]), and waits to be called (`clew-speak listen <model dir>
//! <keyword>…`, [`listen`]).
//!
//! A request that cannot be met is answered `{"failed":{"id":1,"error":…}}`;
//! a model that cannot be loaded, `{"error":…}` before it exits. It exits
//! when stdin closes.

mod embed;
mod listen;
mod mel;
mod runtime;
mod turn;

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
    let mut samples = audio.samples().to_vec();
    if family == Family::Kokoro {
        without_hop_whine(&mut samples, tts.sample_rate());
    }
    Ok(samples)
}

/// How narrow each notch of [`without_hop_whine`] is: its centre over the
/// width it takes out.
const WHINE_Q: f64 = 20.0;

/// Kokoro's int8 model whines at a fifth and two fifths of its rate — 4.8
/// and 9.6 kHz at its 24 kHz — from its vocoder's five-sample hop, which
/// int8 weights leave a steady tone of: measured 15 to 25 dB above what is
/// around it, all through the speech and the pauses between (the full-size
/// model, three times the download, about 5). Two narrow notches there, run
/// forward and back so nothing is shifted in time, take it out and leave
/// the rest of the voice within a fraction of a dB. (The pattern's other
/// harmonics fold onto these two below the Nyquist frequency.)
fn without_hop_whine(samples: &mut [f32], rate: i32) {
    if rate <= 0 {
        return;
    }
    let rate = f64::from(rate);
    for fifths in [1.0, 2.0] {
        let (b, a) = notch(rate * fifths / 5.0, rate, WHINE_Q);
        biquad(samples, b, a);
        samples.reverse();
        biquad(samples, b, a);
        samples.reverse();
    }
}

/// A notch at `centre` Hz (the audio EQ cookbook's), normalized: the
/// numerator and the denominator's last two.
fn notch(centre: f64, rate: f64, q: f64) -> ([f64; 3], [f64; 2]) {
    let w = std::f64::consts::TAU * centre / rate;
    let alpha = w.sin() / (2.0 * q);
    let a0 = 1.0 + alpha;
    let cos = w.cos();
    (
        [1.0 / a0, -2.0 * cos / a0, 1.0 / a0],
        [-2.0 * cos / a0, (1.0 - alpha) / a0],
    )
}

/// `samples` through the biquad `b`/`a`, in place.
fn biquad(samples: &mut [f32], b: [f64; 3], a: [f64; 2]) {
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    for sample in samples.iter_mut() {
        let x = f64::from(*sample);
        let y = b[0] * x + b[1] * x1 + b[2] * x2 - a[0] * y1 - a[1] * y2;
        (x2, x1, y2, y1) = (x1, x, y1, y);
        *sample = y as f32;
    }
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
    /// a smoother voice — in the language it is told; Kokoro reads its
    /// language from its lexicons and has no steps to set.
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
        let config = super::generation(super::Family::Kokoro, &say(Some("zh")), 4);
        assert_eq!(
            config.num_steps,
            sherpa_onnx::GenerationConfig::default().num_steps
        );
        assert_eq!(config.sid, 3, "a voice it has");
        assert!(config.extra.is_none());
    }

    use super::*;

    /// How strong `hz` is in `samples` at `rate`, in dB, over their middle
    /// half (the filters' edges left out).
    fn level(samples: &[f32], rate: f64, hz: f64) -> f64 {
        let (from, to) = (samples.len() / 4, samples.len() * 3 / 4);
        let (mut re, mut im) = (0.0, 0.0);
        for (n, &s) in samples.iter().enumerate().take(to).skip(from) {
            let phase = std::f64::consts::TAU * hz * n as f64 / rate;
            re += f64::from(s) * phase.cos();
            im += f64::from(s) * phase.sin();
        }
        10.0 * ((re * re + im * im) / ((to - from) as f64).powi(2)).log10()
    }

    /// Kokoro's whine at a fifth and two fifths of its rate is taken out;
    /// the voice around it — low, middle, high, and right beside the
    /// notches — is left as it was.
    #[test]
    fn the_hop_whine_is_taken_out_and_the_voice_left() {
        let rate = 24_000.0;
        let probes = [
            200.0, 1_000.0, 3_000.0, 4_300.0, 7_000.0, 10_400.0, 11_000.0,
        ];
        let whine = [4_800.0, 9_600.0];
        let mut samples: Vec<f32> = (0..24_000)
            .map(|n| {
                let t = f64::from(n) / rate;
                probes
                    .iter()
                    .chain(&whine)
                    .map(|hz| 0.05 * (std::f64::consts::TAU * hz * t).sin())
                    .sum::<f64>() as f32
            })
            .collect();
        let before: Vec<f64> = probes.iter().map(|&hz| level(&samples, rate, hz)).collect();
        let whine_before: Vec<f64> = whine.iter().map(|&hz| level(&samples, rate, hz)).collect();
        without_hop_whine(&mut samples, rate as i32);
        for (hz, was) in probes.iter().zip(before) {
            let now = level(&samples, rate, *hz);
            assert!(
                (now - was).abs() < 0.5,
                "{hz} Hz went from {was:.1} to {now:.1} dB"
            );
        }
        for (hz, was) in whine.iter().zip(whine_before) {
            let now = level(&samples, rate, *hz);
            assert!(
                was - now > 40.0,
                "{hz} Hz only from {was:.1} to {now:.1} dB"
            );
        }
    }

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
