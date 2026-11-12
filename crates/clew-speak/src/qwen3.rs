//! Qwen3-TTS (Qwen, Apache-2.0), the 0.6B model with voices of its own
//! ("CustomVoice"): speech as a person says it, in English, Chinese,
//! Japanese, Korean, German, French, Spanish, Portuguese, Italian and
//! Russian — and streamed, its first fraction of a second played while the
//! rest is still being made.
//!
//! It is run as published in ONNX by Quick Inference (Apache-2.0): its
//! weights quantized, the model in five graphs.
//!
//! - The **talker**, a 28-layer transformer, reads the sentence and makes
//!   speech as codes, one frame of 80 ms at a time: each frame's first code.
//! - The **code predictor**, five layers, makes the frame's other fifteen
//!   codes from the talker's state, one after another.
//! - Two tables of **embeddings**: the text's, the codes'.
//! - The **decoder** makes 24 kHz audio of the codes. It is causal — what it
//!   makes of a frame depends on the frames before it only — so audio is
//!   made a few frames at a time as they come, each run given the 25 frames
//!   before it again for context, as the model's own streaming does.
//!
//! The prompt is the model's own (`qwen_tts`): the sentence in its chat
//! form, the language and the speaker as codes, and the whole sentence's
//! text before the first frame is made. Its talker's graph asks the
//! runtime for something the CPU's code cannot do, and is changed as it
//! loads to what the CPU can ([`crate::graph`]).

use std::borrow::Cow;
use std::path::Path;
use std::sync::mpsc;
use std::time::Instant;

use ort::session::{Session, SessionInputValue};
use ort::value::{DynValue, Tensor};
use tokenizers::Tokenizer;

use crate::stretch::Stretch;

/// The rate it speaks at, in Hz.
pub const RATE: i32 = 24_000;
/// Its voices, by number: 0 is a voice of each language's own (Ryan's for
/// English, Vivian's for Chinese, Ono Anna's for Japanese, Sohee's for
/// Korean), then Ryan, Aiden, Vivian, Serena, Uncle Fu, Ono Anna and Sohee
/// in every language.
pub const VOICES: i32 = 8;

/// The files of its folder.
pub const TALKER: &str = "talker/model.onnx";
pub const TALKER_WEIGHTS: &str = "talker/model.onnx.data";
pub const PREDICTOR: &str = "code_predictor/model.onnx";
pub const PREDICTOR_WEIGHTS: &str = "code_predictor/model.onnx.data";
/// The name both graphs give their weights' file.
const WEIGHTS_NAME: &str = "model.onnx.data";
pub const TEXT_EMBEDDINGS: &str = "text_embed_q4.onnx";
pub const CODE_EMBEDDINGS: &str = "codec_embed_q4.onnx";
pub const DECODER: &str = "tok_decoder_q4c8.onnx";
pub const TOKENIZER: &str = "tokenizer.json";

/// How wide its states are.
const WIDTH: usize = 1024;
/// Samples to a frame (80 ms at 24 kHz), and its seconds.
const FRAME: usize = 1920;
const FRAME_SECONDS: f32 = 0.08;
/// Codes to a frame, and how many each of them can be.
const CODEBOOKS: usize = 16;
const CODES: usize = 2048;
/// The talker's codes: the first codebook's, then its markers.
const TALKER_CODES: usize = 3072;
const TALKER_LAYERS: usize = 28;
const PREDICTOR_LAYERS: usize = 5;
const KV_HEADS: usize = 8;
const HEAD: usize = 128;

/// The text's markers.
const TTS_PAD: u32 = 151_671;
const TTS_BOS: u32 = 151_672;
const TTS_EOS: u32 = 151_673;
/// The codes' markers.
const CODE_PAD: i64 = 2148;
const CODE_BOS: i64 = 2149;
const CODE_EOS: usize = 2150;
const THINK: i64 = 2154;
const NO_THINK: i64 = 2155;
const THINK_BOS: i64 = 2156;
const THINK_EOS: i64 = 2157;

/// How its codes are drawn: the model's own settings.
const TEMPERATURE: f32 = 0.9;
const TOP_K: usize = 50;
const REPETITION_PENALTY: f32 = 1.05;
/// The frames a sentence must have before it may end.
const FEWEST_FRAMES: usize = 2;

/// Frames decoded first — 320 ms, so speech starts soon — and the most at
/// once after: each run is given [`CONTEXT`] frames again, so fewer, longer
/// runs cost less once speech is under way.
const FIRST_CHUNK: usize = 4;
const LARGEST_CHUNK: usize = 48;
const CONTEXT: usize = 25;

/// How much longer than what is made a sentence is taken to need, made
/// slower than it plays, before its speech is let go: some to spare.
const MARGIN: f32 = 1.15;
/// How much longer than what is made so far a sentence that has run past
/// what it was expected to be is taken to be.
const OVERRUN: f32 = 1.25;

/// The speakers, as the model numbers them, in the order of [`VOICES`].
const RYAN: i64 = 3061;
const AIDEN: i64 = 2861;
const VIVIAN: i64 = 3065;
const SERENA: i64 = 3066;
const UNCLE_FU: i64 = 3010;
const ONO_ANNA: i64 = 2873;
const SOHEE: i64 = 2864;
const SPEAKERS: [i64; 7] = [RYAN, AIDEN, VIVIAN, SERENA, UNCLE_FU, ONO_ANNA, SOHEE];

/// `lang` without its region (`zh-TW` is `zh`), in lower case.
fn base(lang: &str) -> String {
    lang.split(['-', '_'])
        .next()
        .unwrap_or(lang)
        .to_ascii_lowercase()
}

/// The language as the model numbers it, if it speaks it.
fn language(lang: &str) -> Option<i64> {
    Some(match base(lang).as_str() {
        "en" => 2050,
        "de" => 2053,
        "es" => 2054,
        "zh" => 2055,
        "ja" => 2058,
        "fr" => 2061,
        "ko" => 2064,
        "ru" => 2069,
        "it" => 2070,
        "pt" => 2071,
        _ => return None,
    })
}

/// Who says it: voice `voice` ([`VOICES`]) in `lang`.
fn speaker(voice: i32, lang: &str) -> i64 {
    match usize::try_from(voice) {
        Ok(v @ 1..=7) => SPEAKERS[v - 1],
        _ => match base(lang).as_str() {
            "zh" => VIVIAN,
            "ja" => ONO_ANNA,
            "ko" => SOHEE,
            _ => RYAN,
        },
    }
}

/// The codes before a sentence's text: no thinking, its language, who says
/// it, and the start of speech.
fn prompt_codes(voice: i32, lang: &str) -> Vec<i64> {
    let mut codes = match language(lang) {
        Some(lang) => vec![THINK, THINK_BOS, lang, THINK_EOS],
        None => vec![NO_THINK, THINK_BOS, THINK_EOS],
    };
    codes.extend([speaker(voice, lang), CODE_PAD, CODE_BOS]);
    codes
}

/// About how long `text` takes to say, in seconds — on the long side: CJK
/// characters a syllable or two each, kana one, letters a fifteenth of a
/// second, a pause at each mark that ends a clause.
fn spoken_seconds(text: &str) -> f32 {
    0.3 + text
        .chars()
        .map(|c| match c as u32 {
            0x3040..=0x30ff => 0.14,
            0x4e00..=0x9fff | 0x3400..=0x4dbf | 0xac00..=0xd7af | 0xf900..=0xfaff => 0.25,
            _ if c.is_alphanumeric() => 0.07,
            _ if ".,;:!?。，、；：！？".contains(c) => 0.25,
            _ => 0.03,
        })
        .sum::<f32>()
}

/// Random numbers for drawing codes: SplitMix64.
struct Rng(u64);

impl Rng {
    fn seeded() -> Rng {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Rng(now ^ u64::from(std::process::id()) << 32)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// In [0, 1).
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// A code drawn from `logits`: one of the [`TOP_K`] likeliest, as likely
/// as the model makes it at [`TEMPERATURE`].
fn draw(logits: &[f32], rng: &mut Rng) -> usize {
    let mut top: Vec<usize> = (0..logits.len()).collect();
    let k = TOP_K.min(top.len());
    if k == 0 {
        return 0;
    }
    top.select_nth_unstable_by(k - 1, |&a, &b| logits[b].total_cmp(&logits[a]));
    top.truncate(k);
    let best = top
        .iter()
        .map(|&i| logits[i])
        .fold(f32::NEG_INFINITY, f32::max);
    if !best.is_finite() {
        return top[0];
    }
    let weights: Vec<f32> = top
        .iter()
        .map(|&i| ((logits[i] - best) / TEMPERATURE).exp())
        .collect();
    let mut at = rng.unit() * weights.iter().sum::<f32>();
    for (&code, weight) in top.iter().zip(&weights) {
        if at < *weight {
            return code;
        }
        at -= weight;
    }
    top[k - 1]
}

/// A frame's first code, from the talker's `logits`: of the first
/// codebook's codes or the end of speech — not before [`FEWEST_FRAMES`] —
/// with codes already said made a little less likely.
fn first_code(logits: &mut [f32], said: &[bool], frame: usize, rng: &mut Rng) -> usize {
    for (code, logit) in logits.iter_mut().enumerate().skip(CODES) {
        if code != CODE_EOS || frame < FEWEST_FRAMES {
            *logit = f32::NEG_INFINITY;
        }
    }
    for (logit, _) in logits.iter_mut().zip(said).filter(|(_, said)| **said) {
        *logit = if *logit < 0.0 {
            *logit * REPETITION_PENALTY
        } else {
            *logit / REPETITION_PENALTY
        };
    }
    draw(logits, rng)
}

fn failed(e: impl std::fmt::Display) -> String {
    format!("the voice failed: {e}")
}

/// A graph that keeps what it has seen (its keys and values), given what
/// comes next a step at a time.
struct Cached {
    session: Session,
    layers: usize,
    past: Vec<(String, DynValue)>,
    seen: usize,
}

impl Cached {
    fn new(session: Session, layers: usize) -> Cached {
        Cached {
            session,
            layers,
            past: Vec::new(),
            seen: 0,
        }
    }

    /// Forget all it has seen.
    fn reset(&mut self) -> Result<(), String> {
        self.past.clear();
        self.seen = 0;
        for layer in 0..self.layers {
            for kind in ["key", "value"] {
                let empty = Tensor::<f32>::from_array(([1usize, KV_HEADS, 0, HEAD], Vec::new()))
                    .map_err(failed)?;
                self.past
                    .push((format!("past_key_values.{layer}.{kind}"), empty.into_dyn()));
            }
        }
        Ok(())
    }

    /// `embeds` — rows of [`WIDTH`] — seen next: its state after the last of
    /// them, and its logits there.
    fn step(&mut self, embeds: Vec<f32>) -> Result<(Vec<f32>, Vec<f32>), String> {
        let rows = embeds.len() / WIDTH;
        self.seen += rows;
        let mut inputs: Vec<(Cow<'static, str>, SessionInputValue<'static>)> =
            Vec::with_capacity(2 + self.past.len());
        inputs.push((
            "inputs_embeds".into(),
            Tensor::from_array(([1usize, rows, WIDTH], embeds))
                .map_err(failed)?
                .into(),
        ));
        inputs.push((
            "attention_mask".into(),
            Tensor::from_array(([1usize, self.seen], vec![1i64; self.seen]))
                .map_err(failed)?
                .into(),
        ));
        let names: Vec<String> = self.past.iter().map(|(name, _)| name.clone()).collect();
        for (name, value) in self.past.drain(..) {
            inputs.push((name.into(), value.into()));
        }
        let mut outputs = self.session.run(inputs).map_err(failed)?;
        let last = |name: &str| -> Result<Vec<f32>, String> {
            let (shape, data) = outputs[name].try_extract_tensor::<f32>().map_err(failed)?;
            let width = shape.last().map_or(0, |&w| w.max(0) as usize);
            if width == 0 || data.len() < width {
                return Err(failed(format!("no {name}")));
            }
            Ok(data[data.len() - width..].to_vec())
        };
        let state = last("hidden_states")?;
        let logits = last("logits")?;
        for name in names {
            let present = name.replacen("past_key_values.", "present.", 1);
            let value = outputs
                .remove(&present)
                .ok_or_else(|| failed(format!("no {present}")))?;
            self.past.push((name, value));
        }
        Ok((state, logits))
    }
}

/// A table of embeddings: one row of [`WIDTH`] for each id.
fn embed(table: &mut Session, ids: &[i64]) -> Result<Vec<f32>, String> {
    let ids = Tensor::from_array(([1usize, ids.len()], ids.to_vec())).map_err(failed)?;
    let outputs = table.run(ort::inputs!["ids" => ids]).map_err(failed)?;
    let (_, rows) = outputs["embeds"]
        .try_extract_tensor::<f32>()
        .map_err(failed)?;
    Ok(rows.to_vec())
}

fn add(into: &mut [f32], row: &[f32]) {
    for (a, b) in into.iter_mut().zip(row) {
        *a += b;
    }
}

fn row(rows: &[f32], i: usize) -> &[f32] {
    &rows[i * WIDTH..(i + 1) * WIDTH]
}

/// When a sentence's speech is let go: as soon as it is made, while it is
/// made faster than it plays; made slower, only once so much is made that
/// what is left will be made before it is needed — so it never stops
/// halfway to wait.
struct Pace {
    /// When making began, once the sentence was read.
    began: Instant,
    /// Seconds of speech the sentence is taken to be.
    expected: f32,
    /// Seconds made.
    made: f32,
    /// What is held back; none once speech is let go.
    held: Option<Vec<f32>>,
}

impl Pace {
    fn new(expected: f32) -> Pace {
        Pace {
            began: Instant::now(),
            expected,
            made: 0.0,
            held: Some(Vec::new()),
        }
    }

    /// Whether, made at `spent` seconds for each second, `made` seconds of
    /// a sentence of `expected` are enough to play while the rest is made.
    fn enough(made: f32, expected: f32, spent: f32) -> bool {
        if made <= 0.0 {
            return false;
        }
        let per_second = spent / made;
        if per_second <= 1.0 {
            return true;
        }
        // Played from now, it runs out when the rest takes longer to make
        // than all that is made takes to play: made ≥ (1 − 1/r) of it all.
        // Past what was expected, a quarter more is taken to be coming.
        let all = expected.max(made * OVERRUN);
        made >= (1.0 - 1.0 / per_second) * all * MARGIN
    }

    fn give(
        &mut self,
        samples: Vec<f32>,
        out: &mut dyn FnMut(&[f32]) -> Result<(), String>,
    ) -> Result<(), String> {
        if samples.is_empty() {
            return Ok(());
        }
        self.made += samples.len() as f32 / RATE as f32;
        let Some(held) = &mut self.held else {
            return out(&samples);
        };
        held.extend(samples);
        if Pace::enough(self.made, self.expected, self.began.elapsed().as_secs_f32()) {
            let held = self.held.take().unwrap_or_default();
            return out(&held);
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut dyn FnMut(&[f32]) -> Result<(), String>) -> Result<(), String> {
        match self.held.take() {
            Some(held) if !held.is_empty() => out(&held),
            _ => Ok(()),
        }
    }
}

/// The decoder, on a thread of its own: one chunk of codes is made audio
/// while the next is made — the code predictor's many small steps leave
/// the cores room for it.
struct Decoder {
    jobs: Option<mpsc::Sender<(Vec<i64>, usize, usize)>>,
    done: mpsc::Receiver<Result<Vec<f32>, String>>,
    /// Chunks sent and not yet come back.
    waiting: usize,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Decoder {
    fn start(mut session: Session) -> Result<Decoder, String> {
        let (jobs, inbox) = mpsc::channel::<(Vec<i64>, usize, usize)>();
        let (outbox, done) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("clew-speak-decoder".into())
            .spawn(move || {
                for (codes, frames, skip) in inbox {
                    let audio = decode(&mut session, codes, frames, skip);
                    if outbox.send(audio).is_err() {
                        break;
                    }
                }
            })
            .map_err(failed)?;
        Ok(Decoder {
            jobs: Some(jobs),
            done,
            waiting: 0,
            worker: Some(worker),
        })
    }

    /// Make audio of `frames` from `from` on, with the [`CONTEXT`] frames
    /// before it.
    fn send(&mut self, frames: &[[i64; CODEBOOKS]], from: usize) -> Result<(), String> {
        let start = from.saturating_sub(CONTEXT);
        let n = frames.len() - start;
        let mut codes = vec![0i64; CODEBOOKS * n];
        for (t, frame) in frames[start..].iter().enumerate() {
            for (book, &code) in frame.iter().enumerate() {
                codes[book * n + t] = code;
            }
        }
        let jobs = self.jobs.as_ref().ok_or_else(|| failed("no decoder"))?;
        jobs.send((codes, n, (from - start) * FRAME))
            .map_err(|_| failed("the decoder stopped"))?;
        self.waiting += 1;
        Ok(())
    }

    /// The chunks made since asked last, in order; all that are still to
    /// come too if `all`. A chunk that failed fails the lot, but only once
    /// every chunk that has come back is taken off the queue, so none of
    /// this sentence's audio is left to be played with the next one's.
    fn made(&mut self, all: bool) -> Result<Vec<Vec<f32>>, String> {
        let mut made = Vec::new();
        let mut error = None;
        while self.waiting > 0 {
            let next = if all {
                self.done
                    .recv()
                    .map_err(|_| failed("the decoder stopped"))?
            } else {
                match self.done.try_recv() {
                    Ok(next) => next,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(failed("the decoder stopped"));
                    }
                }
            };
            self.waiting -= 1;
            match next {
                Ok(audio) => made.push(audio),
                Err(e) => {
                    error.get_or_insert(e);
                }
            }
        }
        match error {
            Some(e) => Err(e),
            None => Ok(made),
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// `frames` frames of codes (`codes`, codebook by codebook) made audio, its
/// first `skip` samples — the context's — left out.
fn decode(
    session: &mut Session,
    codes: Vec<i64>,
    frames: usize,
    skip: usize,
) -> Result<Vec<f32>, String> {
    let codes = Tensor::from_array(([1usize, CODEBOOKS, frames], codes)).map_err(failed)?;
    let outputs = session
        .run(ort::inputs!["codes" => codes])
        .map_err(failed)?;
    let (_, audio) = outputs["audio"]
        .try_extract_tensor::<f32>()
        .map_err(failed)?;
    Ok(audio.get(skip..).unwrap_or_default().to_vec())
}

pub struct Qwen3 {
    tokenizer: Tokenizer,
    talker: Cached,
    predictor: Cached,
    text: Session,
    codes: Session,
    decoder: Decoder,
    rng: Rng,
}

impl Qwen3 {
    /// Load the model in `dir`. Its files are read and let go of: what runs
    /// is the copy in memory.
    pub fn load(dir: &Path) -> Result<Qwen3, String> {
        let unreadable =
            |file: &str, e: std::io::Error| format!("the voice could not be loaded: {file}: {e}");
        let read = |file: &str| std::fs::read(dir.join(file)).map_err(|e| unreadable(file, e));
        let weights = |file: &str| {
            crate::runtime::read_shared(&dir.join(file)).map_err(|e| unreadable(file, e))
        };
        let tokenizer = Tokenizer::from_bytes(read(TOKENIZER)?)
            .map_err(|e| format!("the voice's tokenizer could not be read: {e}"))?;
        let (talker, _) = crate::graph::unfuse_qk_norm(&read(TALKER)?)?;
        let talker = crate::runtime::session_from_memory(
            &talker,
            vec![(WEIGHTS_NAME, weights(TALKER_WEIGHTS)?)],
        )?;
        let predictor = crate::runtime::session_from_memory(
            &read(PREDICTOR)?,
            vec![(WEIGHTS_NAME, weights(PREDICTOR_WEIGHTS)?)],
        )?;
        let session = |file: &str| crate::runtime::session_from_memory(&read(file)?, Vec::new());
        Ok(Qwen3 {
            tokenizer,
            talker: Cached::new(talker, TALKER_LAYERS),
            predictor: Cached::new(predictor, PREDICTOR_LAYERS),
            text: session(TEXT_EMBEDDINGS)?,
            codes: session(CODE_EMBEDDINGS)?,
            decoder: Decoder::start(session(DECODER)?)?,
            rng: Rng::seeded(),
        })
    }

    /// `text` said in `lang` by voice `voice` at `speed`, given to `out` a
    /// part at a time as it is made ([`Pace`]): one channel at [`RATE`].
    pub fn say(
        &mut self,
        text: &str,
        lang: &str,
        voice: i32,
        speed: f32,
        out: &mut dyn FnMut(&[f32]) -> Result<(), String>,
    ) -> Result<(), String> {
        // What a sentence given up on left the decoder making is not this
        // one's.
        let _ = self.decoder.made(true);
        let chat = format!("<|im_start|>assistant\n{text}<|im_end|>\n<|im_start|>assistant\n");
        let ids: Vec<i64> = self
            .tokenizer
            .encode(chat, false)
            .map_err(|e| format!("the text could not be read: {e}"))?
            .get_ids()
            .iter()
            .map(|&id| i64::from(id))
            .collect();
        // The role (3), the text, and the chat's end and next turn (5).
        if ids.len() <= 8 {
            return Ok(());
        }
        let said = &ids[3..ids.len() - 5];
        let markers = embed(
            &mut self.text,
            &[i64::from(TTS_BOS), i64::from(TTS_EOS), i64::from(TTS_PAD)],
        )?;
        let (bos, eos, pad) = (row(&markers, 0), row(&markers, 1), row(&markers, 2));
        let code_ids = prompt_codes(voice, lang);
        let codes = embed(&mut self.codes, &code_ids)?;
        // The role; the codes but the last over padding, the last of them
        // over the start of speech; the text and its end over padding; then
        // the start of speech.
        let mut prompt = embed(&mut self.text, &ids[..3])?;
        let n = code_ids.len();
        for i in 0..n - 1 {
            let mut r = row(&codes, i).to_vec();
            add(&mut r, if i < n - 2 { pad } else { bos });
            prompt.extend(r);
        }
        let mut body = embed(&mut self.text, said)?;
        body.extend_from_slice(eos);
        let code_pad = embed(&mut self.codes, &[CODE_PAD])?;
        for r in body.chunks_exact_mut(WIDTH) {
            add(r, &code_pad);
        }
        prompt.extend(body);
        let mut start = pad.to_vec();
        add(&mut start, row(&codes, n - 1));
        prompt.extend(start);
        let pad = pad.to_vec();

        self.talker.reset()?;
        let (mut state, mut logits) = self.talker.step(prompt)?;
        let expected = spoken_seconds(text);
        let most = (expected / FRAME_SECONDS) as usize * 2 + 40;
        let mut pace = Pace::new(expected / speed.clamp(0.5, 2.0));
        let mut stretch = Stretch::new(speed);
        let mut frames: Vec<[i64; CODEBOOKS]> = Vec::new();
        let mut said_codes = vec![false; CODES];
        let (mut decoded, mut chunk) = (0, FIRST_CHUNK);
        for frame in 0..most {
            let first = first_code(&mut logits, &said_codes, frame, &mut self.rng);
            if first == CODE_EOS {
                break;
            }
            said_codes[first] = true;
            let mut codes = [0i64; CODEBOOKS];
            codes[0] = first as i64;
            let first_embed = embed(&mut self.codes, &[first as i64])?;
            let mut next = first_embed.clone();
            self.predictor.reset()?;
            let mut input = std::mem::take(&mut state);
            input.extend_from_slice(&first_embed);
            let (_, mut predicted) = self.predictor.step(input)?;
            for book in 1..CODEBOOKS {
                let heads = &predicted[(book - 1) * CODES..book * CODES];
                let code = draw(heads, &mut self.rng);
                codes[book] = code as i64;
                let id = (TALKER_CODES + (book - 1) * CODES + code) as i64;
                let code_embed = embed(&mut self.codes, &[id])?;
                add(&mut next, &code_embed);
                if book + 1 < CODEBOOKS {
                    predicted = self.predictor.step(code_embed)?.1;
                }
            }
            frames.push(codes);
            add(&mut next, &pad);
            (state, logits) = self.talker.step(next)?;
            if frames.len() - decoded >= chunk {
                self.decoder.send(&frames, decoded)?;
                decoded = frames.len();
                chunk = (chunk * 2).min(LARGEST_CHUNK);
            }
            for audio in self.decoder.made(false)? {
                pace.give(stretch.push(&audio), out)?;
            }
        }
        if decoded < frames.len() {
            self.decoder.send(&frames, decoded)?;
        }
        for audio in self.decoder.made(true)? {
            pace.give(stretch.push(&audio), out)?;
        }
        pace.give(stretch.finish(), out)?;
        pace.finish(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chunk that failed doesn't leave the chunks after it queued: the
    /// next sentence's first ask finds nothing of this one.
    #[test]
    fn a_failed_chunk_leaves_nothing_behind() {
        let (back, done) = mpsc::channel();
        let mut decoder = Decoder {
            jobs: None,
            done,
            waiting: 3,
            worker: None,
        };
        back.send(Ok(vec![0.1])).unwrap();
        back.send(Err("bad chunk".to_string())).unwrap();
        back.send(Ok(vec![0.3])).unwrap();
        assert_eq!(decoder.made(false), Err("bad chunk".to_string()));
        assert_eq!(decoder.waiting, 0);
        back.send(Ok(vec![0.4])).unwrap();
        decoder.waiting = 1;
        assert_eq!(decoder.made(true), Ok(vec![vec![0.4]]));
        // Waiting for all of them gives the same.
        decoder.waiting = 2;
        back.send(Err("bad".to_string())).unwrap();
        back.send(Ok(vec![0.6])).unwrap();
        assert_eq!(decoder.made(true), Err("bad".to_string()));
        assert_eq!(decoder.waiting, 0);
        assert!(decoder.done.try_recv().is_err());
    }

    /// Each language it speaks is told it by its own code; one it does not
    /// is left to the model to hear; a region is no other language.
    #[test]
    fn its_prompt_names_the_language_and_the_voice() {
        assert_eq!(
            prompt_codes(0, "en"),
            [THINK, THINK_BOS, 2050, THINK_EOS, RYAN, CODE_PAD, CODE_BOS]
        );
        assert_eq!(prompt_codes(0, "zh-TW")[2], 2055);
        assert_eq!(prompt_codes(0, "ja")[4], ONO_ANNA);
        assert_eq!(prompt_codes(0, "ko")[4], SOHEE);
        assert_eq!(prompt_codes(0, "zh")[4], VIVIAN);
        assert_eq!(prompt_codes(0, "fr")[4], RYAN);
        assert_eq!(
            prompt_codes(0, "fi"),
            [NO_THINK, THINK_BOS, THINK_EOS, RYAN, CODE_PAD, CODE_BOS]
        );
        // A voice picked speaks every language; one it has not is the
        // language's own.
        for (voice, speaker) in (1..=7).zip(SPEAKERS) {
            assert_eq!(prompt_codes(voice, "ja")[4], speaker);
        }
        assert_eq!(prompt_codes(8, "ja")[4], ONO_ANNA);
        assert_eq!(prompt_codes(-1, "ko")[4], SOHEE);
        assert_eq!(SPEAKERS.len() as i32 + 1, VOICES);
        for lang in ["en", "zh", "ja", "ko", "de", "fr", "es", "pt", "it", "ru"] {
            assert!(language(lang).is_some(), "{lang}");
        }
    }

    /// The first code is one of the first codebook's, or — after the
    /// fewest frames — the end; codes said already are a little less
    /// likely; the most likely of all is drawn when the rest are far less.
    #[test]
    fn the_first_code_is_drawn_as_the_model_does() {
        let mut rng = Rng(1);
        let mut logits = vec![0.0f32; TALKER_CODES];
        logits[CODE_EOS] = 100.0;
        logits[2500] = 90.0;
        logits[7] = 50.0;
        assert_eq!(
            first_code(&mut logits.clone(), &[false; CODES], 0, &mut rng),
            7
        );
        assert_eq!(
            first_code(&mut logits.clone(), &[false; CODES], 1, &mut rng),
            7
        );
        assert_eq!(
            first_code(&mut logits.clone(), &[false; CODES], 2, &mut rng),
            CODE_EOS
        );
        let mut said = vec![false; CODES];
        said[7] = true;
        let mut l = logits.clone();
        first_code(&mut l, &said, 0, &mut rng);
        assert_eq!(l[7], 50.0 / REPETITION_PENALTY);
        assert_eq!(l[2500], f32::NEG_INFINITY);
        let mut l = vec![-2.0f32; TALKER_CODES];
        first_code(&mut l, &said, 0, &mut rng);
        assert_eq!(l[7], -2.0 * REPETITION_PENALTY);
        assert_eq!(l[8], -2.0);
    }

    /// Drawn often, codes come as often as the model makes them likely,
    /// and only from the likeliest fifty.
    #[test]
    fn codes_are_drawn_from_the_likeliest() {
        let mut rng = Rng(42);
        let mut logits = vec![-1e9f32; 300];
        logits[3] = 0.0;
        logits[4] = (2.0f32).ln() * TEMPERATURE; // twice as likely as 3
        for (i, l) in logits.iter_mut().enumerate().skip(100).take(60) {
            *l = -50.0 - i as f32; // 60 more, far less likely
        }
        let mut count = [0usize; 300];
        for _ in 0..30_000 {
            count[draw(&logits, &mut rng)] += 1;
        }
        let ratio = count[4] as f32 / count[3] as f32;
        assert!((1.85..2.15).contains(&ratio), "{ratio}");
        assert_eq!(count.iter().filter(|&&c| c > 0).count(), 2);
        // Only the top fifty are ever drawn.
        let ramp: Vec<f32> = (0..300).map(|i| i as f32 * 0.01).collect();
        for _ in 0..2_000 {
            assert!(draw(&ramp, &mut rng) >= 250);
        }
    }

    /// Made faster than it plays, speech goes at once; slower, it is held
    /// until what is made will last while the rest is made.
    #[test]
    fn speech_is_held_only_while_it_would_run_out() {
        // Twice as fast as it plays: at once.
        assert!(Pace::enough(0.32, 10.0, 0.16));
        // Nothing made: nothing to play.
        assert!(!Pace::enough(0.0, 10.0, 0.5));
        // Half as fast: half of a 10 s sentence, and some, is needed.
        assert!(!Pace::enough(3.0, 10.0, 6.0));
        assert!(!Pace::enough(5.0, 10.0, 10.0));
        assert!(Pace::enough(5.8, 10.0, 11.6));
        // A sentence that runs past what was expected is taken to go on a
        // quarter more: let go if made twice as slowly as it plays, held if
        // ten times.
        assert!(Pace::enough(9.0, 2.0, 18.0));
        assert!(!Pace::enough(9.0, 2.0, 90.0));
        assert!(!Pace::enough(9.0, 2.0, 60.0));
        // Given out: held, then all at once, then each part as it comes.
        let given = std::cell::RefCell::new(Vec::new());
        let mut out = |s: &[f32]| {
            given.borrow_mut().push(s.len());
            Ok(())
        };
        let mut pace = Pace::new(1.0);
        pace.began = Instant::now() - std::time::Duration::from_secs(60);
        pace.give(vec![0.0; 2400], &mut out).unwrap();
        assert!(
            given.borrow().is_empty(),
            "held: made far slower than it plays"
        );
        pace.finish(&mut out).unwrap();
        assert_eq!(*given.borrow(), [2400], "let go once all is made");
        given.borrow_mut().clear();
        let mut pace = Pace::new(1.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        pace.give(vec![0.0; 24_000], &mut out).unwrap();
        pace.give(vec![0.0; 100], &mut out).unwrap();
        pace.finish(&mut out).unwrap();
        assert_eq!(
            *given.borrow(),
            [24_000, 100],
            "made far faster: each at once"
        );
    }

    /// A real model, when the test is pointed at it (`CLEW_QWEN3_MODEL`):
    /// its talker loads changed for the CPU; a sentence comes in parts, as
    /// long as speech of it is; each of Clew's own languages is spoken; a
    /// faster pace is shorter.
    #[test]
    #[ignore = "needs CLEW_QWEN3_MODEL"]
    fn a_real_voice_speaks_in_parts() {
        let dir = std::path::PathBuf::from(std::env::var("CLEW_QWEN3_MODEL").unwrap());
        let mut voice = Qwen3::load(&dir).unwrap();
        let say = |voice: &mut Qwen3, text: &str, lang: &str, speed: f32| {
            let mut parts = Vec::new();
            voice
                .say(text, lang, 0, speed, &mut |part| {
                    parts.push(part.len());
                    Ok(())
                })
                .unwrap();
            (
                parts.len(),
                parts.iter().sum::<usize>() as f32 / RATE as f32,
            )
        };
        let (parts, seconds) = say(
            &mut voice,
            "The total adds every price, then rounds it to cents.",
            "en",
            1.0,
        );
        assert!(parts >= 2, "{parts} parts");
        assert!((2.0..9.0).contains(&seconds), "{seconds} s");
        for (text, lang) in [
            ("合计会把每个价格加起来。", "zh"),
            ("合計はすべての価格を足します。", "ja"),
            ("합계는 모든 가격을 더합니다.", "ko"),
            ("Le total additionne chaque prix.", "fr"),
        ] {
            let (_, seconds) = say(&mut voice, text, lang, 1.0);
            assert!((0.8..9.0).contains(&seconds), "{lang}: {seconds} s");
        }
        let (_, slow) = say(&mut voice, "One, two, three, four, five.", "en", 1.0);
        let (_, fast) = say(&mut voice, "One, two, three, four, five.", "en", 1.5);
        assert!(fast < slow, "{fast} s at 1.5× against {slow} s");
    }

    /// A sentence is taken to be as long as its words and its script say,
    /// on the long side of what the voices take.
    #[test]
    fn a_sentence_is_taken_as_long_as_it_takes_to_say() {
        let en = spoken_seconds(
            "Here's what this function does. It reads the config file, checks every field, \
             and returns an error the moment something looks wrong.",
        );
        assert!((8.0..13.0).contains(&en), "{en}");
        let zh = spoken_seconds("这个函数先读取配置文件，逐项检查每个字段。");
        assert!((4.0..7.5).contains(&zh), "{zh}");
        let ja = spoken_seconds("この関数は設定ファイルを読み込みます。");
        assert!((2.5..5.5).contains(&ja), "{ja}");
    }
}
