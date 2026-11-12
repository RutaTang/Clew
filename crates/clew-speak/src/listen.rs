//! Waiting to be called (`clew-speak listen <model dir> <keyword>…`): a
//! small keyword spotter — sherpa-onnx's, a streaming Zipformer that knows
//! English and Mandarin — hears what the microphone does, and says only when
//! one of the phrases it was given was said. Nothing else is worked out,
//! kept or written down: it is not a speech recognizer.
//!
//! Each keyword is one argument, written as the model's own sounds — English
//! in ARPAbet, Mandarin in tone-marked pinyin — and named after an `@`:
//! `HH EY1 K L UW1 @hey_clew`. Audio comes at 16 kHz, one channel, as
//! little-endian `f32`s, in pieces as it is heard; nothing is answered until
//! a phrase is heard, and then its name:
//!
//! ```text
//! → {"ready":{"keywords":2}}
//! ← {"hear":{"audio":"<base64: f32 little-endian, 16 kHz>"}}
//! → {"heard":{"keyword":"hey_clew"}}
//! ```
//!
//! `{"reset":{}}` lets go of what was heard so far, and is answered
//! `{"reset":{}}` once it has: whatever was answered before it is about
//! what came before.

use std::io::{BufRead, Write};
use std::path::Path;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// The model's files: the streaming Zipformer cut in chunks of 16 frames,
/// its encoder and joiner quantized.
pub const ENCODER: &str = "encoder-epoch-13-avg-2-chunk-16-left-64.int8.onnx";
pub const DECODER: &str = "decoder-epoch-13-avg-2-chunk-16-left-64.onnx";
pub const JOINER: &str = "joiner-epoch-13-avg-2-chunk-16-left-64.int8.onnx";
pub const TOKENS: &str = "tokens.txt";

/// The rate it hears at.
pub const RATE: i32 = 16_000;

/// The spotter, and the stream of what it has heard.
pub struct Spotter {
    spotter: sherpa_onnx::KeywordSpotter,
    stream: sherpa_onnx::OnlineStream,
}

impl Spotter {
    /// Load the model in `dir`, to listen for `keywords` (one per line, as
    /// the module describes).
    pub fn load(dir: &Path, keywords: &[String]) -> Result<Spotter, String> {
        use sherpa_onnx::*;
        if keywords.is_empty() {
            return Err("no phrase to listen for".into());
        }
        let file = |name: &str| -> Result<String, String> {
            let path = dir.join(name);
            if path.is_file() {
                Ok(path.to_string_lossy().into_owned())
            } else {
                Err(format!("the model has no {name}"))
            }
        };
        let mut config = KeywordSpotterConfig::default();
        config.model_config.transducer = OnlineTransducerModelConfig {
            encoder: Some(file(ENCODER)?),
            decoder: Some(file(DECODER)?),
            joiner: Some(file(JOINER)?),
        };
        config.model_config.tokens = Some(file(TOKENS)?);
        config.model_config.num_threads = 1;
        config.model_config.provider = Some("cpu".into());
        config.keywords_buf = Some(keywords.join("\n"));
        let spotter = KeywordSpotter::create(&config).ok_or_else(|| {
            "the model could not be loaded, or a phrase is not in its sounds".to_string()
        })?;
        let stream = spotter.create_stream();
        Ok(Spotter { spotter, stream })
    }

    /// Hear `samples` (16 kHz) after what was heard before: the name of the
    /// phrase they end, if one was said.
    pub fn hear(&mut self, samples: &[f32]) -> Option<String> {
        self.stream.accept_waveform(RATE, samples);
        let mut heard = None;
        while self.spotter.is_ready(&self.stream) {
            self.spotter.decode(&self.stream);
            if let Some(result) = self.spotter.get_result(&self.stream)
                && !result.keyword.is_empty()
            {
                heard = Some(result.keyword);
                // Heard once: what comes next is listened to afresh.
                self.spotter.reset(&self.stream);
            }
        }
        heard
    }

    /// Let go of what was heard so far.
    pub fn reset(&mut self) {
        self.stream = self.spotter.create_stream();
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Request {
    Hear { audio: String },
    Reset {},
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Answer {
    Ready { keywords: usize },
    Heard { keyword: String },
    Reset {},
    Error(String),
}

/// Samples from their little-endian bytes, in base64.
fn decode(audio: &str) -> Result<Vec<f32>, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(audio)
        .map_err(|e| format!("the audio is unreadable: {e}"))?;
    if bytes.len() % 4 != 0 {
        return Err("the audio is cut short".into());
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

/// Load the model in `dir`, then listen for `keywords` in what is sent
/// until stdin closes.
pub fn run(dir: &Path, keywords: &[String]) -> i32 {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut spotter = match Spotter::load(dir, keywords) {
        Ok(spotter) => spotter,
        Err(e) => {
            let _ = send(&mut out, &Answer::Error(e));
            return 1;
        }
    };
    let ready = Answer::Ready {
        keywords: keywords.len(),
    };
    if send(&mut out, &ready).is_err() {
        return 0;
    }
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Hear { audio }) => match decode(&audio) {
                Ok(samples) => spotter
                    .hear(&samples)
                    .map(|keyword| Answer::Heard { keyword }),
                Err(e) => Some(Answer::Error(e)),
            },
            Ok(Request::Reset {}) => {
                spotter.reset();
                Some(Answer::Reset {})
            }
            Err(e) => Some(Answer::Error(format!("not a request: {e}"))),
        };
        if let Some(answer) = answer
            && send(&mut out, &answer).is_err()
        {
            break;
        }
    }
    0
}

fn send(out: &mut impl Write, answer: &Answer) -> std::io::Result<()> {
    let line = serde_json::to_string(answer).map_err(std::io::Error::other)?;
    writeln!(out, "{line}")?;
    out.flush()
}
