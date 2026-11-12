//! Speech made faster or slower without its pitch changing, as it streams:
//! WSOLA (waveform-similarity overlap-add). Each short window of the output
//! is the stretch of the input that best continues the window before it,
//! looked for near where the pace puts it; windows overlap by half under a
//! Hann window, which sums to one.
//!
//! For the voices whose model has no pace of its own to set.

/// A window: 30 ms at 24 kHz.
const WINDOW: usize = 720;
/// How far apart output windows are: half a window.
const HOP: usize = WINDOW / 2;
/// How far from where the pace puts a window it may be taken from, either
/// way, to continue the one before it smoothly.
const SEEK: usize = HOP / 2;

pub struct Stretch {
    /// Input per output: above 1 faster.
    speed: f64,
    /// The input not yet wholly used, and where it starts in all the input.
    input: Vec<f32>,
    base: usize,
    /// Where the last window was taken from, in all the input; none yet.
    last: Option<usize>,
    /// Output windows placed so far.
    windows: usize,
    /// The output not yet final: the second half of the last window placed.
    tail: Vec<f32>,
    /// Output given so far.
    given: usize,
    hann: Vec<f32>,
}

impl Stretch {
    /// Speech played at `speed` (1 is as made; 1.25 a quarter faster).
    pub fn new(speed: f32) -> Stretch {
        let hann = (0..WINDOW)
            .map(|i| {
                let x = std::f32::consts::PI * i as f32 / WINDOW as f32;
                x.sin() * x.sin()
            })
            .collect();
        Stretch {
            speed: f64::from(speed.clamp(0.5, 2.0)),
            input: Vec::new(),
            base: 0,
            last: None,
            windows: 0,
            tail: vec![0.0; HOP],
            given: 0,
            hann,
        }
    }

    fn unchanged(&self) -> bool {
        (self.speed - 1.0).abs() < 1e-3
    }

    /// More input; what of the output is final now.
    pub fn push(&mut self, samples: &[f32]) -> Vec<f32> {
        if self.unchanged() {
            return samples.to_vec();
        }
        self.input.extend_from_slice(samples);
        let mut out = Vec::new();
        while let Some(window) = self.next_window(false) {
            out.extend(window);
        }
        self.given += out.len();
        out
    }

    /// The input ended: the rest of the output.
    pub fn finish(&mut self) -> Vec<f32> {
        if self.unchanged() {
            return Vec::new();
        }
        let mut out = Vec::new();
        while let Some(window) = self.next_window(true) {
            out.extend(window);
        }
        out.extend(std::mem::take(&mut self.tail));
        // As long as the input was, at the pace.
        let total = self.base + self.input.len();
        let want = (total as f64 / self.speed).round() as usize;
        out.truncate(want.saturating_sub(self.given));
        self.given += out.len();
        self.input.clear();
        out
    }

    /// The next window placed — its first half now final — if the input
    /// reaches far enough for it (or, at the end, at all).
    fn next_window(&mut self, ending: bool) -> Option<Vec<f32>> {
        let ideal = (self.windows as f64 * HOP as f64 * self.speed).round() as usize;
        let end = self.base + self.input.len();
        if ideal >= end {
            return None;
        }
        // Where the last window's input went on: what this one should match.
        let follow = self.last.map(|p| p + HOP);
        let reach = ideal + SEEK + WINDOW;
        let need = follow.map_or(reach, |f| reach.max(f + WINDOW));
        if !ending && need > end {
            return None;
        }
        let at = match follow {
            None => ideal,
            Some(follow) => {
                let low = ideal.saturating_sub(SEEK).max(self.base);
                let high = ideal + SEEK;
                let mut best = (f32::NEG_INFINITY, ideal.max(self.base));
                for at in low..=high {
                    let score = self.similarity(at, follow);
                    if score > best.0 {
                        best = (score, at);
                    }
                }
                best.1
            }
        };
        let mut window = vec![0.0; WINDOW];
        for (i, w) in window.iter_mut().enumerate() {
            *w = self.sample(at + i) * self.hann[i];
        }
        // The first half completes the tail; the second becomes it.
        let done: Vec<f32> = self
            .tail
            .iter()
            .zip(&window[..HOP])
            .map(|(a, b)| a + b)
            .collect();
        self.tail = window[HOP..].to_vec();
        self.last = Some(at);
        self.windows += 1;
        // Input no window will look at again.
        let keep_from = (at + HOP).min(ideal.saturating_sub(SEEK)).min(at);
        if keep_from > self.base {
            self.input.drain(..keep_from - self.base);
            self.base = keep_from;
        }
        Some(done)
    }

    fn sample(&self, at: usize) -> f32 {
        at.checked_sub(self.base)
            .and_then(|i| self.input.get(i))
            .copied()
            .unwrap_or(0.0)
    }

    /// How alike the input at `a` and at `b` is, over a window: their
    /// correlation, every other sample.
    fn similarity(&self, a: usize, b: usize) -> f32 {
        (0..WINDOW)
            .step_by(2)
            .map(|i| self.sample(a + i) * self.sample(b + i))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, seconds: f32) -> Vec<f32> {
        (0..(24_000.0 * seconds) as usize)
            .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / 24_000.0).sin() * 0.5)
            .collect()
    }

    /// Times the signal crosses zero going up, per second: its pitch.
    fn pitch(samples: &[f32]) -> f32 {
        let ups = samples
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        ups as f32 / (samples.len() as f32 / 24_000.0)
    }

    fn stretched(speed: f32, input: &[f32], chunk: usize) -> Vec<f32> {
        let mut s = Stretch::new(speed);
        let mut out = Vec::new();
        for part in input.chunks(chunk) {
            out.extend(s.push(part));
        }
        out.extend(s.finish());
        out
    }

    /// Faster or slower, it lasts as long as the pace says, at the same
    /// pitch.
    #[test]
    fn the_pace_changes_and_the_pitch_does_not() {
        let input = tone(220.0, 2.0);
        for speed in [0.8, 1.25, 1.5] {
            let out = stretched(speed, &input, 4096);
            let want = input.len() as f32 / speed;
            assert!(
                (out.len() as f32 - want).abs() < 0.01 * want,
                "{speed}: {} for {want}",
                out.len()
            );
            let p = pitch(&out[HOP..out.len() - HOP]);
            assert!((p - 220.0).abs() < 4.0, "{speed}: pitch {p}");
            // No sample beyond what the tone had: windows that overlap add
            // to one.
            let peak = out.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(peak < 0.56, "{speed}: peak {peak}");
        }
    }

    /// At its own pace, what comes in goes out as it was.
    #[test]
    fn at_its_own_pace_nothing_changes() {
        let input = tone(330.0, 0.5);
        assert_eq!(stretched(1.0, &input, 1000), input);
    }

    /// Given in pieces of any size, the same comes out as given at once.
    #[test]
    fn pieces_make_the_same_as_the_whole() {
        let input: Vec<f32> = tone(180.0, 1.0)
            .iter()
            .zip(tone(523.0, 1.0))
            .map(|(a, b)| a + b * 0.3)
            .collect();
        let whole = stretched(1.25, &input, input.len());
        for chunk in [1, 333, 7680] {
            assert_eq!(stretched(1.25, &input, chunk), whole, "pieces of {chunk}");
        }
        // Nothing in, nothing out.
        assert!(stretched(1.25, &[], 10).is_empty());
    }
}
