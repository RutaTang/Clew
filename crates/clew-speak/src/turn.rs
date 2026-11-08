//! Whether a speaker has finished (`clew-speak turn <model file>`): Smart
//! Turn v3 — Whisper Tiny's encoder with a classifier on it — hears the
//! last eight seconds of what was said and answers how likely it is that
//! the speaker is done, from how it was said: the fall of the voice, the
//! pace, a word left hanging. A pause to think is not the end of a turn,
//! and a short answer is not cut off by waiting for more.
//!
//! Audio comes at 16 kHz, one channel, as little-endian `f32`s; what it
//! answers is the probability that the turn is over:
//!
//! ```text
//! → {"ready":{"seconds":8}}
//! ← {"turn":{"id":1,"audio":"<base64: f32 little-endian, 16 kHz>"}}
//! → {"done":{"id":1,"p":0.97}}
//! ```

use std::io::{BufRead, Write};
use std::path::Path;

use base64::Engine as _;
use ort::session::Session;
use ort::value::Tensor;
use serde::{Deserialize, Serialize};

use crate::mel;

/// How much of what was said it hears: the last eight seconds.
pub const SECONDS: usize = 8;

pub struct Judge {
    session: Session,
}

impl Judge {
    /// Load the model in `file`. It is read and let go of: what runs is the
    /// copy in memory.
    pub fn load(file: &Path) -> Result<Judge, String> {
        let model =
            std::fs::read(file).map_err(|e| format!("the model could not be loaded: {e}"))?;
        let session = crate::runtime::session_from_memory(&model, Vec::new())?;
        let mut judge = Judge { session };
        // It answers, and in range, before it is said to be ready.
        judge.judge(&[])?;
        Ok(judge)
    }

    /// How likely it is, 0 to 1, that the speaker of `samples` (16 kHz)
    /// has finished.
    pub fn judge(&mut self, samples: &[f32]) -> Result<f32, String> {
        let frames = mel::frames(SECONDS);
        let features = mel::features(samples, SECONDS);
        let input = Tensor::from_array(([1usize, mel::BANDS, frames], features))
            .map_err(|e| e.to_string())?;
        let outputs = self
            .session
            .run(ort::inputs!["input_features" => input])
            .map_err(|e| format!("the model failed: {e}"))?;
        let (_, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("the model made no answer: {e}"))?;
        match data.first() {
            Some(&p) if (0.0..=1.0).contains(&p) => Ok(p),
            Some(p) => Err(format!("the model answered {p}, not a probability")),
            None => Err("the model made no answer".into()),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Request {
    Turn { id: u64, audio: String },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Answer {
    Ready { seconds: usize },
    Done { id: u64, p: f32 },
    Failed { id: u64, error: String },
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

/// Load the model in `file`, then judge what is asked until stdin closes.
pub fn run(file: &Path) -> i32 {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut judge = match Judge::load(file) {
        Ok(judge) => judge,
        Err(e) => {
            let _ = send(&mut out, &Answer::Error(e));
            return 1;
        }
    };
    if send(&mut out, &Answer::Ready { seconds: SECONDS }).is_err() {
        return 0;
    }
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Turn { id, audio }) => match decode(&audio).and_then(|s| judge.judge(&s)) {
                Ok(p) => Answer::Done { id, p },
                Err(error) => Answer::Failed { id, error },
            },
            Err(e) => Answer::Error(format!("not a request: {e}")),
        };
        if send(&mut out, &answer).is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Audio reads back as the samples it was made of; bytes that are not
    /// whole samples are refused, not half read.
    #[test]
    fn audio_reads_back() {
        let samples = [0.0f32, 0.5, -1.0];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let audio = base64::engine::general_purpose::STANDARD.encode(&bytes);
        assert_eq!(decode(&audio).unwrap(), samples);
        let cut = base64::engine::general_purpose::STANDARD.encode(&bytes[..5]);
        assert!(decode(&cut).is_err());
    }

    /// The protocol's lines, as the client reads them.
    #[test]
    fn answers_are_one_line_each() {
        let mut out = Vec::new();
        send(&mut out, &Answer::Ready { seconds: 8 }).unwrap();
        send(&mut out, &Answer::Done { id: 3, p: 0.5 }).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"ready\":{\"seconds\":8}}\n{\"done\":{\"id\":3,\"p\":0.5}}\n"
        );
    }
}
