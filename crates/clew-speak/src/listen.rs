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
use std::time::{Duration, Instant};

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

/// The spotter, and the streams of what it has heard.
///
/// The model hears a phrase less well at some places in its stream than at
/// others: said in a stretch of about 0.4 s that comes round every 1.6 s
/// or so, "Hey, Clew" was heard half the time or less, and elsewhere
/// nineteen times in twenty — so whether it was heard came down to when it
/// was said. What is heard goes to two streams, the second
/// [`STAGGER_SECONDS`] further on in its own time, so that the phrase is
/// never in such a stretch in both; it is heard when either hears it.
pub struct Spotter {
    spotter: sherpa_onnx::KeywordSpotter,
    streams: [sherpa_onnx::OnlineStream; 2],
    /// What is heard, brought up to the level the model knows speech at.
    level: Level,
}

/// What the microphone hears, brought up to the level speech is said at
/// close by: the model hears "Hey, Clew" said softly, or across the room,
/// far less often than said aloud — at −53 dBFS one time in six in a
/// quiet room, and with a little noise one in three — and as often as
/// aloud once it is brought up. The gain follows the loudest of what was
/// said lately: down at once, for what is loud; up slowly, a few dB a
/// second, so that the pauses between words are not raised with it; never
/// below 1 nor above [`Level::MAX_GAIN`].
#[derive(Debug, Clone)]
pub struct Level {
    /// The loudest 10 ms heard lately, as RMS: what fell back since.
    peak: f32,
    gain: f32,
    /// When something was last heard.
    heard_at: Option<Instant>,
}

impl Level {
    /// The level the loudest 10 ms of speech is brought to, as RMS: about
    /// −20 dBFS.
    const TARGET: f32 = 0.1;
    /// The most it is brought up by: +24 dB.
    const MAX_GAIN: f32 = 16.0;
    /// How long the loudest of what was heard takes to fall back by half.
    const HALF_LIFE_SECONDS: f32 = 4.0;
    /// How fast the gain rises, in dB a second.
    const RISE_DB: f32 = 6.0;
    /// The length it is measured in: 10 ms at [`RATE`].
    const FRAME: usize = RATE as usize / 100;
    /// Longer than this with nothing heard — waiting off while Clew
    /// listened, or spoke — is taken for as long a quiet: pieces come a
    /// fifth of a second apart while it waits.
    const UNHEARD: Duration = Duration::from_secs(1);

    pub fn new() -> Level {
        Level {
            peak: Self::TARGET / Self::MAX_GAIN,
            gain: 1.0,
            heard_at: None,
        }
    }

    /// `samples` (at [`RATE`]), brought up: 10 ms at a time.
    pub fn apply(&mut self, samples: &[f32]) -> Vec<f32> {
        self.apply_at(samples, Instant::now())
    }

    /// `samples`, heard at `now`, brought up. A while with nothing heard
    /// before them counts as quiet: a loud "Hey, Clew" — or a door shut —
    /// before Clew listened does not hold the level down minutes later,
    /// as though it had just been heard.
    fn apply_at(&mut self, samples: &[f32], now: Instant) -> Vec<f32> {
        if let Some(at) = self.heard_at.replace(now) {
            let unheard = now.saturating_duration_since(at);
            if unheard > Self::UNHEARD {
                self.rest(unheard.as_secs_f32());
            }
        }
        let frame_seconds = Self::FRAME as f32 / RATE as f32;
        let fall = 0.5f32.powf(frame_seconds / Self::HALF_LIFE_SECONDS);
        let rise = 10f32.powf(Self::RISE_DB * frame_seconds / 20.0);
        let mut out = Vec::with_capacity(samples.len());
        for frame in samples.chunks(Self::FRAME) {
            let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
            self.peak = rms.max(self.peak * fall);
            let wanted =
                (Self::TARGET / self.peak.max(f32::MIN_POSITIVE)).clamp(1.0, Self::MAX_GAIN);
            self.gain = if wanted < self.gain {
                wanted
            } else {
                wanted.min(self.gain * rise)
            };
            out.extend(frame.iter().map(|s| (s * self.gain).clamp(-1.0, 1.0)));
        }
        out
    }

    /// `seconds` of quiet: the loudest heard falls back, and the gain rises
    /// toward what that wants, as heard 10 ms at a time.
    fn rest(&mut self, seconds: f32) {
        self.peak *= 0.5f32.powf(seconds / Self::HALF_LIFE_SECONDS);
        let wanted = (Self::TARGET / self.peak.max(f32::MIN_POSITIVE)).clamp(1.0, Self::MAX_GAIN);
        let rise = 10f32.powf(Self::RISE_DB * seconds / 20.0);
        self.gain = wanted.min(self.gain * rise);
    }
}

impl Default for Level {
    fn default() -> Self {
        Level::new()
    }
}

/// How much further on the second stream is than the first: half the
/// distance between the stretches where the phrase is heard less well.
pub const STAGGER_SECONDS: f32 = 0.8;

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
        let streams = streams(&spotter);
        Ok(Spotter {
            spotter,
            streams,
            level: Level::new(),
        })
    }

    /// Hear `samples` (16 kHz) after what was heard before: the name of the
    /// phrase they end, if one was said. Heard, both streams begin afresh:
    /// the other, which may hear it a moment later, does not say it again.
    pub fn hear(&mut self, samples: &[f32]) -> Option<String> {
        let samples = self.level.apply(samples);
        let mut heard = None;
        for stream in &self.streams {
            stream.accept_waveform(RATE, &samples);
            while self.spotter.is_ready(stream) {
                self.spotter.decode(stream);
                if let Some(result) = self.spotter.get_result(stream)
                    && !result.keyword.is_empty()
                {
                    heard.get_or_insert(result.keyword);
                    self.spotter.reset(stream);
                }
            }
        }
        if heard.is_some() {
            self.reset();
        }
        heard
    }

    /// Let go of what was heard so far — not of how loud the room is.
    pub fn reset(&mut self) {
        self.streams = streams(&self.spotter);
    }
}

/// Two fresh streams, the second [`STAGGER_SECONDS`] further on: it has
/// heard that much silence before what both hear next.
fn streams(spotter: &sherpa_onnx::KeywordSpotter) -> [sherpa_onnx::OnlineStream; 2] {
    let first = spotter.create_stream();
    let second = spotter.create_stream();
    second.accept_waveform(RATE, &vec![0.0; (RATE as f32 * STAGGER_SECONDS) as usize]);
    [first, second]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// A voice-like tone, `seconds` long, as loud as `amp`.
    fn voice(seconds: f32, amp: f32) -> Vec<f32> {
        (0..(RATE as f32 * seconds) as usize)
            .map(|i| amp * (i as f32 * 220.0 * std::f32::consts::TAU / RATE as f32).sin())
            .collect()
    }

    /// Speech said softly is brought up, a few dB a second, to the level
    /// speech is said at close by — no further.
    #[test]
    fn soft_speech_is_brought_up_slowly() {
        let mut level = Level::new();
        // −53 dBFS: after a second, raised some; after six, as far as it goes.
        let soft = voice(1.0, 0.0032);
        let first = level.apply(&soft);
        let gain = rms(&first[first.len() - 160..]) / rms(&soft[soft.len() - 160..]);
        assert!((1.5..2.5).contains(&gain), "{gain}");
        for _ in 0..5 {
            level.apply(&soft);
        }
        let later = level.apply(&soft);
        let gain = rms(&later) / rms(&soft);
        assert!(
            (Level::MAX_GAIN - 0.5..=Level::MAX_GAIN).contains(&gain),
            "{gain}"
        );
    }

    /// Loud speech after soft is taken down at once — not a frame let
    /// through raised — and never below how it was said.
    #[test]
    fn loud_speech_is_taken_down_at_once_and_never_below_itself() {
        let mut level = Level::new();
        for _ in 0..6 {
            level.apply(&voice(1.0, 0.0032));
        }
        let loud = voice(0.5, 0.5);
        let out = level.apply(&loud);
        let worst = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(worst <= 0.5 + 1e-6, "{worst}");
        assert!((rms(&out) - rms(&loud)).abs() < 1e-4, "as it was said");
    }

    /// A while with nothing heard is taken for quiet: a loud sound just
    /// before still holds soft speech down, one a minute before does not.
    #[test]
    fn a_while_unheard_is_taken_for_quiet() {
        let soft = voice(0.2, 0.0032);
        let start = Instant::now();
        let mut level = Level::new();
        level.apply_at(&voice(0.5, 0.3), start);
        let mut soon = level.clone();
        let out = soon.apply_at(&soft, start + Duration::from_millis(700));
        assert!(rms(&out) / rms(&soft) < 1.5, "{}", rms(&out) / rms(&soft));
        let out = level.apply_at(&soft, start + Duration::from_secs(60));
        let gain = rms(&out) / rms(&soft);
        assert!(gain > Level::MAX_GAIN - 0.5, "{gain}");
    }

    /// Between words, what is heard is not raised past where the words
    /// were: the gain follows the loudest of what was said lately.
    #[test]
    fn a_pause_is_not_raised_with_the_words() {
        let mut level = Level::new();
        let mut said = voice(1.0, 0.02);
        said.extend(vec![0.0005; RATE as usize]);
        for _ in 0..6 {
            level.apply(&said);
        }
        let out = level.apply(&said);
        let second = RATE as usize;
        let words = rms(&out[..second]) / rms(&said[..second]);
        let pause = rms(&out[second..]) / rms(&said[second..]);
        // Up a little as the words are left behind, not to the most it goes.
        assert!(pause < words * 1.3, "pause {pause}, words {words}");
        assert!(pause < Level::MAX_GAIN * 0.6, "{pause}");
    }
}
