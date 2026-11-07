//! Text embedded on this computer, by EmbeddingGemma (`clew-speak embed
//! <model dir>`): what Clew's semantic index and its search are made of
//! when no embedding service is used.
//!
//! The folder holds the model as published (`onnx/model_q4.onnx` and its
//! weights, `tokenizer.json`). Texts come as they are to be embedded — the
//! caller puts the model's prompt before each (`title: none | text: …`,
//! `task: search result | query: …`); vectors go back as the model makes
//! them, each of unit length:
//!
//! ```text
//! → {"ready":{"dims":768}}
//! ← {"embed":{"id":1,"texts":["title: none | text: Adds the prices."]}}
//! → {"vectors":{"id":1,"dims":768,"data":"<base64: f32 little-endian, row by row>"}}
//! ```

use std::io::{BufRead, Write};
use std::path::Path;

use base64::Engine as _;
use ort::session::Session;
use ort::value::Tensor;
use serde::{Deserialize, Serialize};
use tokenizers::Tokenizer;

/// The most tokens of a text that are embedded. Clew's texts — summaries,
/// questions — are far shorter; a batch is as long as its longest text, and
/// at the model's own limit (2,048) one long text made a batch of sixteen
/// take most of a minute and gigabytes.
const MAX_TOKENS: usize = 512;

/// The model's file in the folder, and its weights beside it — under the
/// name the model gives them.
pub const MODEL_FILE: &str = "onnx/model_q4.onnx";
pub const WEIGHTS_FILE: &str = "onnx/model_q4.onnx_data";
const WEIGHTS_NAME: &str = "model_q4.onnx_data";
pub const TOKENIZER_FILE: &str = "tokenizer.json";

/// The tokenizer in `json`, cutting a text at [`MAX_TOKENS`] and padding
/// none: each text of a batch is run as long as it is.
fn tokenizer(json: &[u8]) -> Result<Tokenizer, String> {
    let unreadable = |e: tokenizers::Error| format!("the tokenizer could not be read: {e}");
    let mut tokenizer = Tokenizer::from_bytes(json).map_err(unreadable)?;
    tokenizer
        .with_truncation(Some(tokenizers::TruncationParams {
            max_length: MAX_TOKENS,
            ..Default::default()
        }))
        .map_err(unreadable)?;
    tokenizer.with_padding(None);
    Ok(tokenizer)
}

pub struct Embedder {
    session: Session,
    tokenizer: Tokenizer,
    dims: usize,
}

impl Embedder {
    /// Load the model in `dir`. Its files are read and let go of: what
    /// runs is the copy in memory.
    pub fn load(dir: &Path) -> Result<Embedder, String> {
        let unreadable =
            |file: &str, e: std::io::Error| format!("the model could not be loaded: {file}: {e}");
        let read = |file: &str| std::fs::read(dir.join(file)).map_err(|e| unreadable(file, e));
        let tokenizer = tokenizer(&read(TOKENIZER_FILE)?)?;
        let model = read(MODEL_FILE)?;
        let weights =
            read_shared(&dir.join(WEIGHTS_FILE)).map_err(|e| unreadable(WEIGHTS_FILE, e))?;
        let session = crate::runtime::session_from_memory(&model, vec![(WEIGHTS_NAME, weights)])?;
        let mut embedder = Embedder {
            session,
            tokenizer,
            dims: 0,
        };
        // What it makes, from what it makes: the vector's length.
        embedder.dims = embedder
            .embed(&["title: none | text: ready".to_string()])?
            .first()
            .map_or(0, Vec::len);
        if embedder.dims == 0 {
            return Err("the model made no vector".into());
        }
        Ok(embedder)
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    /// One vector for each of `texts`, of unit length, in their order.
    pub fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| format!("a text could not be read: {e}"))?;
        let width = encodings.iter().map(|e| e.len()).max().unwrap_or(0).max(1);
        let rows = encodings.len();
        // Padded at the end; what is padding is masked out.
        let mut ids = vec![0i64; rows * width];
        let mut mask = vec![0i64; rows * width];
        for (row, encoding) in encodings.iter().enumerate() {
            for (col, (&id, &attend)) in encoding
                .get_ids()
                .iter()
                .zip(encoding.get_attention_mask())
                .enumerate()
            {
                ids[row * width + col] = i64::from(id);
                mask[row * width + col] = i64::from(attend);
            }
        }
        let shape = [rows, width];
        let input_ids = Tensor::from_array((shape, ids)).map_err(|e| e.to_string())?;
        let attention_mask = Tensor::from_array((shape, mask)).map_err(|e| e.to_string())?;
        let outputs = self
            .session
            .run(ort::inputs![
                "input_ids" => input_ids,
                "attention_mask" => attention_mask,
            ])
            .map_err(|e| format!("the model failed: {e}"))?;
        let (shape, data) = outputs["sentence_embedding"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("the model made no vectors: {e}"))?;
        let dims = shape.get(1).copied().unwrap_or(0).max(0) as usize;
        if dims == 0 || data.len() != rows * dims {
            return Err("the model made vectors of no length".into());
        }
        Ok(data.chunks_exact(dims).map(unit).collect())
    }
}

/// The bytes of the file at `path`, read straight into the buffer the
/// model keeps: a file of weights is a fifth of a gigabyte, and read first
/// and then copied it was in memory twice over while the model loaded.
fn read_shared(path: &Path) -> std::io::Result<std::sync::Arc<[u8]>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let len =
        usize::try_from(file.metadata()?.len()).map_err(|_| std::io::Error::other("too large"))?;
    let mut bytes: std::sync::Arc<[u8]> = std::iter::repeat_n(0u8, len).collect();
    let buffer = std::sync::Arc::get_mut(&mut bytes).expect("not shared yet");
    file.read_exact(buffer)?;
    // Grown since it was measured: not the file it was.
    if file.read(&mut [0u8; 1])? != 0 {
        return Err(std::io::Error::other("changed while it was read"));
    }
    Ok(bytes)
}

/// `v` scaled to length 1 (as it is, if it has none).
fn unit(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter().map(|x| x / norm).collect()
    } else {
        v.to_vec()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Request {
    Embed { id: u64, texts: Vec<String> },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Answer {
    Ready { dims: usize },
    Vectors { id: u64, dims: usize, data: String },
    Failed { id: u64, error: String },
    Error(String),
}

/// Vectors as their little-endian bytes, in base64.
fn encode(vectors: &[Vec<f32>]) -> String {
    let bytes: Vec<u8> = vectors
        .iter()
        .flatten()
        .flat_map(|x| x.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Load the model in `dir`, then embed what is asked until stdin closes.
pub fn run(dir: &Path) -> i32 {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut embedder = match Embedder::load(dir) {
        Ok(embedder) => embedder,
        Err(e) => {
            let _ = send(&mut out, &Answer::Error(e));
            return 1;
        }
    };
    if send(
        &mut out,
        &Answer::Ready {
            dims: embedder.dims(),
        },
    )
    .is_err()
    {
        return 0;
    }
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Embed { id, texts }) => match embedder.embed(&texts) {
                Ok(vectors) => Answer::Vectors {
                    id,
                    dims: embedder.dims(),
                    data: encode(&vectors),
                },
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

    /// A long text is cut at the most tokens a text is embedded with, and
    /// a short one is not padded: a batch is as long as its longest text.
    #[test]
    fn a_text_is_cut_at_the_most_tokens() {
        let json = br#"{"version":"1.0","truncation":null,"padding":null,
            "added_tokens":[],"normalizer":null,
            "pre_tokenizer":{"type":"WhitespaceSplit"},"post_processor":null,
            "decoder":null,"model":{"type":"WordLevel",
            "vocab":{"word":0,"[UNK]":1},"unk_token":"[UNK]"}}"#;
        let words = tokenizer(json).unwrap();
        let long = "word ".repeat(3000);
        let batch = words
            .encode_batch(vec![long, "word word".to_string()], true)
            .unwrap();
        assert_eq!(batch[0].get_ids().len(), 512);
        assert_eq!(batch[1].get_ids().len(), 2);
        assert!(tokenizer(b"not a tokenizer").is_err());
    }

    /// A vector is made of unit length; one of none is left as it is.
    #[test]
    fn vectors_are_of_unit_length() {
        let v = unit(&[3.0, 4.0]);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        assert_eq!(unit(&[0.0, 0.0]), [0.0, 0.0]);
    }

    /// Vectors go out as their bytes, row after row.
    #[test]
    fn vectors_are_sent_as_their_bytes() {
        let data = encode(&[vec![1.0, -2.0], vec![0.5, 0.25]]);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap();
        let back: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        assert_eq!(back, [1.0, -2.0, 0.5, 0.25]);
    }

    /// EmbeddingGemma, when the test is pointed at it: texts alike are
    /// nearer than texts apart, each vector of unit length, in the order
    /// asked.
    #[test]
    #[ignore = "needs CLEW_EMBED_MODEL"]
    fn a_real_model_embeds() {
        let dir = std::path::PathBuf::from(std::env::var("CLEW_EMBED_MODEL").unwrap());
        let mut embedder = Embedder::load(&dir).unwrap();
        assert_eq!(embedder.dims(), 768);
        // Loaded, none of its files is held: they can be removed under it.
        #[cfg(target_os = "linux")]
        {
            let held = |list: &str| {
                std::fs::read_dir(list)
                    .unwrap()
                    .flatten()
                    .filter_map(|e| std::fs::read_link(e.path()).ok())
                    .any(|target| target.starts_with(&dir))
            };
            assert!(!held("/proc/self/fd"), "a file of the model is open");
            let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
            assert!(
                !maps.contains(dir.to_str().unwrap()),
                "a file of the model is mapped"
            );
        }
        let texts = [
            "title: none | text: Sums the price of every item in the cart.",
            "task: search result | query: where is the cart total computed?",
            "title: none | text: Opens a connection to the database.",
        ]
        .map(String::from);
        let v = embedder.embed(&texts).unwrap();
        assert_eq!(v.len(), 3);
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        for x in &v {
            assert!((dot(x, x) - 1.0).abs() < 1e-4);
        }
        assert!(dot(&v[1], &v[0]) > dot(&v[1], &v[2]) + 0.1, "{v:?}");
        // One at a time, as in a batch.
        let alone = embedder.embed(&texts[1..2]).unwrap();
        assert!(dot(&alone[0], &v[1]) > 0.999);
    }
}
