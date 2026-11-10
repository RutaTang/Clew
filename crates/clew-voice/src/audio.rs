//! What the microphone hands over, made into what whisper reads: one
//! channel at 16 kHz ([`mono`], [`resample`]), cut to the stretch with
//! speech in it ([`speech`]) — and how loud it is now ([`meter`]), for the
//! level the window shows while it listens.

use std::ops::Range;

/// The sample rate whisper reads.
pub const WHISPER_RATE: u32 = 16_000;

/// Interleaved frames of `channels` channels, averaged into one.
pub fn mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    match channels {
        0 => Vec::new(),
        1 => interleaved.to_vec(),
        n => interleaved
            .chunks_exact(n)
            .map(|frame| frame.iter().sum::<f32>() / n as f32)
            .collect(),
    }
}

/// Root-mean-square amplitude of `samples` (0 for none).
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

/// Quietest level the meter shows, in dBFS; anything below reads as 0.
const METER_FLOOR_DB: f32 = -60.0;

/// An RMS amplitude as a level meter's fill, 0 to 1: −60 dBFS (a quiet
/// room) to 0 dBFS (full scale), linear in decibels, as ears hear it.
pub fn meter(rms: f32) -> f32 {
    if rms <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
}

/// Zero crossings of the windowed sinc on each side of its centre: the
/// filter's length, and so its steepness.
const ZERO_CROSSINGS: f64 = 16.0;
/// Where the low-pass filter starts to fall, as a fraction of the lower
/// Nyquist frequency: just inside it, so what passes cannot alias.
const ROLLOFF: f64 = 0.94;
/// Most fractional delays a filter table holds. A ratio of rates needing
/// more (an odd device rate) rounds the delay to the nearest of this many,
/// a 1/1024-sample step: far below what can be heard.
const MAX_PHASES: u64 = 1024;

/// `input`, sampled at `from` Hz, resampled to `to` Hz.
///
/// Band-limited interpolation with a Blackman-windowed sinc, its cutoff at
/// the lower of the two Nyquist frequencies, so downsampling a 48 kHz
/// microphone does not fold what lies above 8 kHz back into the speech
/// band. The filter is tabulated once per fractional delay (polyphase), so
/// the cost is one multiply-add per tap.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    let mut resampler = Resampler::new(from, to);
    let mut out = resampler.push(input);
    out.extend(resampler.finish());
    out
}

/// [`resample`] for audio that comes in parts — speech as it is made: each
/// sample out is made once all the input its filter reaches is in, so the
/// parts join as if they had been resampled whole, with nothing at the
/// joins; [`Resampler::finish`] makes the last, past the end of the input.
pub struct Resampler {
    /// Rates the same, or one of them none: the input as it is.
    unchanged: bool,
    up: u64,
    down: u64,
    phases: u64,
    half: i64,
    /// table[p][j]: the weight of input sample k0 + j + 1 - half for an
    /// output `p / phases` of a sample past k0, normalized to unity gain.
    table: Vec<Vec<f32>>,
    /// The input still reached by what is to be made, and where in all of
    /// it that starts.
    input: Vec<f32>,
    base: u64,
    /// Input taken in all.
    seen: u64,
    /// The next sample to make.
    next: u64,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Resampler {
        let unchanged = from == to || from == 0 || to == 0;
        let g = gcd(u64::from(from.max(1)), u64::from(to.max(1)));
        let (up, down) = (u64::from(to.max(1)) / g, u64::from(from.max(1)) / g);
        let phases = up.min(MAX_PHASES);
        // Cutoff in cycles per input sample; the kernel's half-width in input
        // samples follows from it.
        let ratio = f64::from(to.max(1)) / f64::from(from.max(1));
        let cutoff = 0.5 * ratio.min(1.0) * ROLLOFF;
        let half_width = ZERO_CROSSINGS / (2.0 * cutoff);
        let half = half_width.ceil() as i64;
        let taps = (2 * half) as usize;
        let table = if unchanged {
            Vec::new()
        } else {
            (0..phases)
                .map(|p| {
                    let frac = p as f64 / phases as f64;
                    let mut row: Vec<f64> = (0..taps)
                        .map(|j| {
                            let d = frac - (j as i64 + 1 - half) as f64;
                            if d.abs() >= half_width {
                                return 0.0;
                            }
                            2.0 * cutoff * sinc(2.0 * cutoff * d) * blackman(d / half_width)
                        })
                        .collect();
                    let sum: f64 = row.iter().sum();
                    if sum.abs() > f64::EPSILON {
                        row.iter_mut().for_each(|w| *w /= sum);
                    }
                    row.into_iter().map(|w| w as f32).collect()
                })
                .collect()
        };
        Resampler {
            unchanged,
            up,
            down,
            phases,
            half,
            table,
            input: Vec::new(),
            base: 0,
            seen: 0,
            next: 0,
        }
    }

    /// Output sample `n`'s filter row, and the first input sample it
    /// reaches.
    fn at(&self, n: u64) -> (usize, i64) {
        // Output sample n sits at input position n * down / up.
        let num = n * self.down;
        let mut k0 = (num / self.up) as i64;
        let rem = num % self.up;
        let mut p = if self.phases == self.up {
            rem
        } else {
            ((rem as f64 / self.up as f64) * self.phases as f64).round() as u64
        };
        if p == self.phases {
            p = 0;
            k0 += 1;
        }
        (p as usize, k0 + 1 - self.half)
    }

    /// Make samples while `ready` says the input for the next is in.
    fn make(&mut self, until: u64, ready: impl Fn(i64) -> bool) -> Vec<f32> {
        let mut out = Vec::new();
        while self.next < until {
            let (p, first) = self.at(self.next);
            if !ready(first) {
                break;
            }
            let mut acc = 0.0f32;
            for (j, w) in self.table[p].iter().enumerate() {
                let k = first + j as i64;
                if k >= self.base as i64
                    && let Some(x) = self.input.get((k - self.base as i64) as usize)
                {
                    acc += x * w;
                }
            }
            out.push(acc);
            self.next += 1;
        }
        // Input none of what is still to be made reaches.
        let (_, first) = self.at(self.next);
        let keep = first.max(0) as u64;
        if keep > self.base {
            let drop = ((keep - self.base) as usize).min(self.input.len());
            self.input.drain(..drop);
            self.base += drop as u64;
        }
        out
    }

    /// More input: what of the output is made of it now.
    pub fn push(&mut self, input: &[f32]) -> Vec<f32> {
        if self.unchanged {
            return input.to_vec();
        }
        self.input.extend_from_slice(input);
        self.seen += input.len() as u64;
        let taps = self.half * 2;
        let seen = self.seen as i64;
        self.make(u64::MAX, |first| first + taps <= seen)
    }

    /// The input ended: the rest of the output, as long as the input was at
    /// the new rate.
    pub fn finish(&mut self) -> Vec<f32> {
        if self.unchanged {
            return Vec::new();
        }
        let until = self.seen * self.up / self.down;
        self.make(until, |_| true)
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// The Blackman window over −1..1, centred on 0.
fn blackman(u: f64) -> f64 {
    use std::f64::consts::PI;
    0.42 + 0.5 * (PI * u).cos() + 0.08 * (2.0 * PI * u).cos()
}

/// Length of one analysis frame at 16 kHz: 30 ms.
const FRAME: usize = 480;
/// The level below which no frame counts as speech, however quiet the
/// room: −50 dBFS.
const SPEECH_MIN: f32 = 0.003_2;
/// The level above which every frame counts as speech, however loud the
/// room seems: −34 dBFS. Without it, a clip that is speech from end to end
/// would read its own voice as the noise floor.
const SPEECH_CAP: f32 = 0.02;
/// How far above the room's own level a frame must be to count, as a
/// factor of amplitude: about +12 dB.
const OVER_FLOOR: f32 = 4.0;
/// Fewest frames above the threshold that make speech: 120 ms. A key's
/// click or a knock on the desk is shorter.
const MIN_VOICED: usize = 4;
/// Kept before the first and after the last frame of speech, so a soft
/// first consonant or trailing syllable is not cut: 300 and 450 ms.
const PAD_BEFORE: usize = 16_000 * 3 / 10;
const PAD_AFTER: usize = 16_000 * 45 / 100;

/// The stretch of `samples` (16 kHz mono) that holds speech, padded a
/// little each side, or `None` when nothing in it sounds like speech.
///
/// An energy gate: frames 30 ms long, the room's level taken as the 10th
/// percentile of theirs, and speech whatever stands well above it. Whisper
/// is not asked to transcribe silence, which it fills with words it never
/// heard ("Thank you.").
pub fn speech(samples: &[f32]) -> Option<Range<usize>> {
    let levels: Vec<f32> = samples.chunks(FRAME).map(rms).collect();
    if levels.is_empty() {
        return None;
    }
    let threshold = threshold(&levels);
    let voiced: Vec<usize> = levels
        .iter()
        .enumerate()
        .filter(|(_, l)| **l >= threshold)
        .map(|(i, _)| i)
        .collect();
    if voiced.len() < MIN_VOICED {
        return None;
    }
    let first = voiced[0] * FRAME;
    let last = ((voiced[voiced.len() - 1] + 1) * FRAME).min(samples.len());
    Some(first.saturating_sub(PAD_BEFORE)..(last + PAD_AFTER).min(samples.len()))
}

/// Length of a level frame, in seconds: what [`speech`] and
/// [`trailing_silence`] read, at any rate.
pub const FRAME_SECONDS: f32 = FRAME as f32 / WHISPER_RATE as f32;

/// The speech threshold for frames at `levels`: well above the room's own
/// level, within fixed bounds.
fn threshold(levels: &[f32]) -> f32 {
    let mut sorted = levels.to_vec();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[sorted.len() / 10];
    (floor * OVER_FLOOR).clamp(SPEECH_MIN, SPEECH_CAP)
}

/// While listening: how long it has been quiet since the speaker last
/// spoke, in seconds, given the level ([`rms`]) of each [`FRAME_SECONDS`]
/// heard so far — or `None` while nothing has sounded like speech. What
/// ends a recording on its own once the speaker has finished.
pub fn trailing_silence(levels: &[f32]) -> Option<f32> {
    if levels.is_empty() {
        return None;
    }
    let threshold = threshold(levels);
    let voiced = levels.iter().filter(|l| **l >= threshold).count();
    if voiced < MIN_VOICED {
        return None;
    }
    let last = levels.iter().rposition(|l| *l >= threshold)?;
    Some((levels.len() - 1 - last) as f32 * FRAME_SECONDS)
}

/// Fewest frames in a row above the threshold that make speech said over
/// Clew: 120 ms unbroken. What is left of Clew's own voice once its echo
/// is taken out comes in shorter bursts — three frames at most, measured
/// through a speaker the microphone hears — and a word, even a short one
/// said softly, is longer.
const OVER_RUN: usize = 4;

/// While Clew speaks: where, in seconds from the start of `levels` (the
/// [`rms`] of each [`FRAME_SECONDS`] heard), speech said over it began —
/// four frames in a row above the threshold, 120 ms — or `None` while
/// nothing has: what holds Clew for the reader. Stricter than
/// [`trailing_silence`], which a few frames anywhere satisfy. Frames heard
/// while the echo canceller was still learning the room — `learning` — do
/// not count: what it lets through of Clew's voice then comes in bursts as
/// long and loud as a word.
pub fn speech_over(levels: &[f32], learning: Range<usize>) -> Option<f32> {
    if levels.is_empty() {
        return None;
    }
    let threshold = threshold(levels);
    let mut run = 0;
    for (i, level) in levels.iter().enumerate() {
        let speech = *level >= threshold && !learning.contains(&i);
        run = if speech { run + 1 } else { 0 };
        if run == OVER_RUN {
            return Some((i + 1 - OVER_RUN) as f32 * FRAME_SECONDS);
        }
    }
    None
}

/// Shortest clip handed to whisper: it refuses less than a second, and
/// ends a phrase more surely with silence after it.
const MIN_CLIP: usize = 16_000 * 5 / 4;

/// A recording at `rate` Hz, one channel, made ready for whisper:
/// resampled to 16 kHz, cut to its speech, padded with silence to at least
/// 1.25 s. `None` when it holds no speech.
pub fn prepare(samples: &[f32], rate: u32) -> Option<Vec<f32>> {
    let audio = resample(samples, rate, WHISPER_RATE);
    let span = speech(&audio)?;
    let mut clip = audio[span].to_vec();
    if clip.len() < MIN_CLIP {
        clip.resize(MIN_CLIP, 0.0);
    }
    Some(clip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn tone(freq: f32, rate: u32, seconds: f32, amp: f32) -> Vec<f32> {
        let n = (rate as f32 * seconds) as usize;
        (0..n)
            .map(|i| amp * (2.0 * PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    /// The amplitude of `freq` in `x` (sampled at `rate`), by correlation
    /// with a sine and a cosine over whole samples (Goertzel's measure).
    fn amplitude_at(x: &[f32], freq: f32, rate: u32) -> f32 {
        let (mut s, mut c) = (0.0f64, 0.0f64);
        for (i, v) in x.iter().enumerate() {
            let t = 2.0 * std::f64::consts::PI * f64::from(freq) * i as f64 / f64::from(rate);
            s += f64::from(*v) * t.sin();
            c += f64::from(*v) * t.cos();
        }
        (2.0 * (s * s + c * c).sqrt() / x.len() as f64) as f32
    }

    #[test]
    fn channels_are_averaged_into_one() {
        assert_eq!(mono(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
        assert_eq!(mono(&[0.25, 0.5], 1), vec![0.25, 0.5]);
        assert_eq!(mono(&[0.3, 0.3, 0.3, 0.9], 4), vec![0.45]);
        assert!(mono(&[1.0], 0).is_empty());
        // A trailing partial frame is dropped, not misread.
        assert_eq!(mono(&[1.0, 1.0, 1.0], 2), vec![1.0]);
    }

    #[test]
    fn the_meter_reads_decibels() {
        assert_eq!(meter(0.0), 0.0);
        assert_eq!(meter(1.0), 1.0);
        assert!((meter(0.1) - 2.0 / 3.0).abs() < 1e-4, "−20 dBFS");
        assert!((meter(0.001) - 0.0).abs() < 1e-4, "−60 dBFS");
        assert_eq!(meter(1e-5), 0.0);
        assert_eq!(meter(4.0), 1.0, "clipping still reads full");
        assert!((rms(&[0.5, -0.5, 0.5, -0.5]) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn the_same_rate_is_left_alone() {
        let x = tone(440.0, 16_000, 0.1, 0.5);
        assert_eq!(resample(&x, 16_000, 16_000), x);
        assert!(resample(&[], 48_000, 16_000).is_empty());
    }

    /// The common microphone rates come out at 16 kHz with the speech band
    /// intact: a 440 Hz and a 3 kHz tone keep their pitch and level.
    #[test]
    fn microphone_rates_resample_to_16k_keeping_the_speech_band() {
        for from in [48_000, 44_100, 32_000, 22_050, 96_000, 8_000, 11_025] {
            for freq in [440.0, 3_000.0] {
                if freq * 2.0 >= from as f32 * 0.94 {
                    continue;
                }
                let x = tone(freq, from, 0.5, 0.5);
                let y = resample(&x, from, WHISPER_RATE);
                let want = (x.len() as u64 * 16_000 / u64::from(from)) as usize;
                assert_eq!(y.len(), want, "{from} Hz: length");
                // Away from the edges, where the filter runs out of input.
                let mid = &y[400..y.len() - 400];
                let a = amplitude_at(mid, freq, WHISPER_RATE);
                assert!((a - 0.5).abs() < 0.02, "{from} Hz, {freq} Hz: level {a}");
            }
        }
    }

    /// What lies above the new Nyquist frequency is filtered out rather
    /// than folded into the speech band: a 12 kHz tone at 48 kHz would
    /// alias to 4 kHz.
    #[test]
    fn what_cannot_be_kept_is_filtered_not_aliased() {
        let x = tone(12_000.0, 48_000, 0.5, 0.5);
        let y = resample(&x, 48_000, WHISPER_RATE);
        let mid = &y[400..y.len() - 400];
        let alias = amplitude_at(mid, 4_000.0, WHISPER_RATE);
        assert!(alias < 0.001, "aliased {alias}");
        assert!(rms(mid) < 0.001, "{}", rms(mid));
        // Just under the cutoff still passes.
        let x = tone(7_000.0, 48_000, 0.5, 0.5);
        let y = resample(&x, 48_000, WHISPER_RATE);
        let a = amplitude_at(&y[400..y.len() - 400], 7_000.0, WHISPER_RATE);
        assert!(a > 0.4, "7 kHz kept at {a}");
    }

    /// An odd device rate needs more fractional delays than the table
    /// holds; rounding them costs nothing audible.
    #[test]
    fn an_odd_rate_resamples_through_rounded_delays() {
        let from = 44_101;
        let x = tone(1_000.0, from, 0.5, 0.5);
        let y = resample(&x, from, WHISPER_RATE);
        let a = amplitude_at(&y[400..y.len() - 400], 1_000.0, WHISPER_RATE);
        assert!((a - 0.5).abs() < 0.02, "{a}");
    }

    /// Upsampling a narrowband microphone keeps its tone and adds nothing
    /// above its own Nyquist frequency.
    /// Resampled a part at a time, as speech is made, the parts join into
    /// exactly what resampling all of it at once makes — for any sizes of
    /// part, up and down.
    #[test]
    fn parts_resample_as_the_whole_does() {
        let input: Vec<f32> = tone(440.0, 24_000, 0.5, 0.5)
            .iter()
            .zip(tone(3_100.0, 24_000, 0.5, 0.2))
            .map(|(a, b)| a + b)
            .collect();
        for (from, to) in [
            (24_000, 48_000),
            (24_000, 44_100),
            (24_000, 16_000),
            (44_100, 24_000),
        ] {
            let whole = resample(&input, from, to);
            for part in [1, 7, 1920, 5000] {
                let mut r = Resampler::new(from, to);
                let mut parts = Vec::new();
                for chunk in input.chunks(part) {
                    parts.extend(r.push(chunk));
                }
                parts.extend(r.finish());
                assert_eq!(parts, whole, "{from} → {to} in parts of {part}");
            }
        }
        // The same rate: as it came, part by part.
        let mut r = Resampler::new(24_000, 24_000);
        assert_eq!(r.push(&input[..10]), &input[..10]);
        assert!(r.finish().is_empty());
    }

    #[test]
    fn upsampling_adds_no_images() {
        let x = tone(1_000.0, 8_000, 0.5, 0.5);
        let y = resample(&x, 8_000, WHISPER_RATE);
        assert_eq!(y.len(), 8_000);
        let mid = &y[400..y.len() - 400];
        assert!((amplitude_at(mid, 1_000.0, WHISPER_RATE) - 0.5).abs() < 0.02);
        // The image of 1 kHz at 8 kHz − 1 kHz = 7 kHz.
        assert!(amplitude_at(mid, 7_000.0, WHISPER_RATE) < 0.002);
    }

    /// A quiet room with a hum, then a voice, then the room again.
    fn room_then_voice(voice_from: f32, voice_to: f32, total: f32) -> Vec<f32> {
        let n = (16_000.0 * total) as usize;
        let mut seed = 7u32;
        (0..n)
            .map(|i| {
                // A cheap deterministic noise generator.
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.002;
                let t = i as f32 / 16_000.0;
                let voice = if (voice_from..voice_to).contains(&t) {
                    0.1 * (2.0 * PI * 180.0 * t).sin() * (1.0 + 0.5 * (2.0 * PI * 3.0 * t).sin())
                } else {
                    0.0
                };
                noise + voice
            })
            .collect()
    }

    #[test]
    fn speech_is_found_and_padded() {
        let x = room_then_voice(1.0, 2.0, 3.0);
        let span = speech(&x).expect("speech");
        // Padded: 300 ms before, 450 ms after, within a frame.
        let (start, end) = (span.start as f32 / 16_000.0, span.end as f32 / 16_000.0);
        assert!((start - 0.7).abs() < 0.04, "starts at {start}");
        assert!((end - 2.45).abs() < 0.04, "ends at {end}");
    }

    #[test]
    fn silence_and_clicks_are_not_speech() {
        assert_eq!(speech(&room_then_voice(0.0, 0.0, 2.0)), None);
        assert_eq!(speech(&vec![0.0; 32_000]), None);
        assert_eq!(speech(&[]), None);
        // A key's click: 40 ms, loud.
        let mut x = room_then_voice(0.0, 0.0, 2.0);
        x[8_000..8_640].iter_mut().for_each(|s| *s = 0.5);
        assert_eq!(speech(&x), None);
    }

    /// Speech from the first sample to the last is still speech: the room's
    /// level is never taken to be the voice's own.
    #[test]
    fn a_clip_that_is_all_speech_is_kept_whole() {
        let x = room_then_voice(0.0, 2.0, 2.0);
        assert_eq!(speech(&x), Some(0..x.len()));
    }

    /// The silence after speech is measured as it grows; before any
    /// speech there is none to measure.
    #[test]
    fn the_silence_after_speech_is_measured() {
        let levels = |x: &[f32]| x.chunks(FRAME).map(rms).collect::<Vec<_>>();
        assert_eq!(trailing_silence(&[]), None);
        assert_eq!(
            trailing_silence(&levels(&room_then_voice(0.0, 0.0, 2.0))),
            None
        );
        // Speech from 0.5 s to 1.5 s, heard up to 2.5 s: a second of quiet.
        let x = room_then_voice(0.5, 1.5, 2.5);
        let quiet = trailing_silence(&levels(&x)).expect("speech");
        assert!((quiet - 1.0).abs() < 0.04, "{quiet}");
        // Still speaking at the end: none.
        let x = room_then_voice(0.5, 2.5, 2.5);
        assert_eq!(trailing_silence(&levels(&x)), Some(0.0));
    }

    /// Said over Clew: speech is a run of frames above the threshold, and
    /// where the run began is where it was said. Bursts shorter than the
    /// run — what is left of Clew's echo — are not, however many.
    #[test]
    fn speech_over_clew_is_an_unbroken_run() {
        let quiet = 0.0005;
        let loud = 0.05;
        assert_eq!(speech_over(&[], 0..0), None);
        // Bursts of three frames, again and again: not speech.
        let bursts: Vec<f32> = (0..60)
            .map(|i| if i % 8 < 3 { loud } else { quiet })
            .collect();
        assert_eq!(speech_over(&bursts, 0..0), None);
        assert!(
            trailing_silence(&bursts).is_some(),
            "the looser test takes them"
        );
        // Four frames in a row from the 40th: speech, from there.
        let mut said = bursts.clone();
        said[40..44].iter_mut().for_each(|l| *l = loud);
        assert_eq!(speech_over(&said, 0..0), Some(40.0 * FRAME_SECONDS));
        // The first run found is where it began.
        said[20..30].iter_mut().for_each(|l| *l = loud);
        assert_eq!(speech_over(&said, 0..0), Some(20.0 * FRAME_SECONDS));
        // What was heard while the canceller learned the room does not
        // count; four frames in a row before it, or after, do.
        assert_eq!(speech_over(&said, 15..41), None);
        assert_eq!(speech_over(&said, 15..40), Some(40.0 * FRAME_SECONDS));
        assert_eq!(speech_over(&said, 24..60), Some(20.0 * FRAME_SECONDS));
        assert_eq!(speech_over(&said, 23..60), None);
    }

    #[test]
    fn prepare_resamples_trims_and_pads() {
        // 48 kHz: half a second of voice in the middle of four seconds.
        let x16 = room_then_voice(2.0, 2.5, 4.0);
        let x48 = resample(&x16, 16_000, 48_000);
        let clip = prepare(&x48, 48_000).expect("speech");
        // 0.5 s of voice + 0.75 s of padding = 1.25 s.
        let secs = clip.len() as f32 / 16_000.0;
        assert!((secs - 1.25).abs() < 0.05, "{secs}");
        // A word too short for whisper is padded with silence to 1.25 s.
        let short = room_then_voice(0.1, 0.25, 0.3);
        let clip = prepare(&short, 16_000).expect("speech");
        assert_eq!(clip.len(), 20_000);
        assert!(clip[short.len()..].iter().all(|s| *s == 0.0));
        assert_eq!(prepare(&vec![0.0; 48_000], 48_000), None);
    }
}
