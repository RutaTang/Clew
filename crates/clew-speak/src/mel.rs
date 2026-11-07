//! Whisper's log-mel spectrogram, made as Hugging Face's
//! `WhisperFeatureExtractor` makes it — 80 bands of a 25 ms window every
//! 10 ms, at 16 kHz — because that is what Smart Turn's encoder was trained
//! on, and it hears nothing else the same way.

/// The rate the audio is at.
pub const RATE: usize = 16_000;
/// The mel bands: the features' rows.
pub const BANDS: usize = 80;
/// The window (25 ms) and the step between windows (10 ms).
const N_FFT: usize = 400;
const HOP: usize = 160;
/// The frequency bins of one window, 0 Hz to 8 kHz.
const BINS: usize = N_FFT / 2 + 1;

/// The frames `seconds` of audio make: a column every 10 ms.
pub fn frames(seconds: usize) -> usize {
    seconds * RATE / HOP
}

/// `samples` (16 kHz, one channel) as the model hears `seconds` of them:
/// the last `seconds`, after silence to make that many if there are fewer;
/// the whole scaled to zero mean and unit variance; then its log-mel bands
/// — [`BANDS`] rows of [`frames`] columns, row by row.
pub fn features(samples: &[f32], seconds: usize) -> Vec<f32> {
    let n = seconds * RATE;
    let mut x = vec![0f64; n];
    let take = samples.len().min(n);
    for (to, &from) in x[n - take..]
        .iter_mut()
        .zip(&samples[samples.len() - take..])
    {
        *to = f64::from(from);
    }
    normalize(&mut x);
    log_mel(&x, frames(seconds))
}

/// `x` less its mean, over its standard deviation.
fn normalize(x: &mut [f64]) {
    let n = x.len().max(1) as f64;
    let mean = x.iter().sum::<f64>() / n;
    let var = x.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n;
    let scale = (var + 1e-7).sqrt();
    for v in x {
        *v = (*v - mean) / scale;
    }
}

/// The log-mel bands of `x`, `frames` of them: a window centred on every
/// 10 ms (the edges mirrored), its power spectrum through the mel filters,
/// in log10 — no lower than 8 below the loudest, and scaled as Whisper's.
fn log_mel(x: &[f64], frames: usize) -> Vec<f32> {
    let window = hann();
    let filters = mel_filters();
    let tw = twiddles();
    let half = N_FFT / 2;
    // Mirrored about the first and the last sample, not repeating them.
    let at = |i: isize| -> f64 {
        let n = x.len() as isize;
        let i = if i < 0 {
            -i
        } else if i >= n {
            2 * (n - 1) - i
        } else {
            i
        };
        x.get(i.clamp(0, n - 1) as usize).copied().unwrap_or(0.0)
    };
    let mut out = vec![0f32; BANDS * frames];
    let mut buffer = [0f64; N_FFT];
    let mut spectrum = [Complex::default(); N_FFT];
    // The silence before what was said is one value throughout: its
    // windows are all alike, worked out once.
    let mut same: Option<(u64, [f64; BANDS])> = None;
    for frame in 0..frames {
        let start = (frame * HOP) as isize - half as isize;
        for (n, b) in buffer.iter_mut().enumerate() {
            *b = at(start + n as isize);
        }
        let flat = buffer.iter().all(|&b| b.to_bits() == buffer[0].to_bits());
        let bands = match same {
            Some((bits, bands)) if flat && bits == buffer[0].to_bits() => bands,
            _ => {
                for (b, w) in buffer.iter_mut().zip(&window) {
                    *b *= w;
                }
                fft(&buffer, &tw, &mut spectrum);
                let power = |k: usize| spectrum[k].re.powi(2) + spectrum[k].im.powi(2);
                let mut bands = [0f64; BANDS];
                for (band, filter) in bands.iter_mut().zip(&filters) {
                    let energy: f64 = filter
                        .weights
                        .iter()
                        .enumerate()
                        .map(|(i, w)| w * power(filter.first + i))
                        .sum();
                    *band = energy.max(1e-10).log10();
                }
                if flat {
                    same = Some((buffer[0].to_bits(), bands));
                }
                bands
            }
        };
        for (m, &band) in bands.iter().enumerate() {
            out[m * frames + frame] = band as f32;
        }
    }
    let top = out.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    for v in &mut out {
        *v = ((*v).max(top - 8.0) + 4.0) / 4.0;
    }
    out
}

/// A periodic Hann window of [`N_FFT`].
fn hann() -> [f64; N_FFT] {
    let mut w = [0f64; N_FFT];
    for (n, v) in w.iter_mut().enumerate() {
        *v = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / N_FFT as f64).cos();
    }
    w
}

/// A complex number, as the transform works with them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Complex {
    re: f64,
    im: f64,
}

impl Complex {
    fn add(self, o: Complex) -> Complex {
        Complex {
            re: self.re + o.re,
            im: self.im + o.im,
        }
    }
    fn mul(self, o: Complex) -> Complex {
        Complex {
            re: self.re * o.re - self.im * o.im,
            im: self.re * o.im + self.im * o.re,
        }
    }
}

/// e^(−2πi·t/N) for every t: the roots of unity every size the transform
/// divides [`N_FFT`] into reads its own from.
fn twiddles() -> [Complex; N_FFT] {
    let mut tw = [Complex::default(); N_FFT];
    for (t, w) in tw.iter_mut().enumerate() {
        let a = -2.0 * std::f64::consts::PI * t as f64 / N_FFT as f64;
        *w = Complex {
            re: a.cos(),
            im: a.sin(),
        };
    }
    tw
}

/// The discrete Fourier transform of `x` into `out`: mixed-radix
/// Cooley–Tukey, as [`N_FFT`] is 2⁴·5² — some 7,000 steps where the sum
/// as written takes 80,000.
fn fft(x: &[f64; N_FFT], tw: &[Complex; N_FFT], out: &mut [Complex; N_FFT]) {
    /// The `n`-point transform of `x[base]`, `x[base + stride]`, … into
    /// `out`: the `p` interleaved `n/p`-point ones, combined.
    fn step(x: &[f64], base: usize, stride: usize, n: usize, tw: &[Complex], out: &mut [Complex]) {
        if n == 1 {
            out[0] = Complex {
                re: x[base],
                im: 0.0,
            };
            return;
        }
        let p = [2, 3, 5]
            .into_iter()
            .find(|&p| n.is_multiple_of(p))
            .unwrap_or(n);
        let m = n / p;
        for r in 0..p {
            step(
                x,
                base + r * stride,
                stride * p,
                m,
                tw,
                &mut out[r * m..(r + 1) * m],
            );
        }
        // X[k + q·m] = Σ_r W_n^(r·(k + q·m)) · Y_r[k], W_n read from the
        // table of N_FFT's roots, every (N_FFT/n)-th.
        let unit = N_FFT / n;
        let mut y = vec![Complex::default(); p];
        for k in 0..m {
            for (r, v) in y.iter_mut().enumerate() {
                *v = out[r * m + k];
            }
            for q in 0..p {
                let at = k + q * m;
                out[at] = y
                    .iter()
                    .enumerate()
                    .fold(Complex::default(), |sum, (r, v)| {
                        sum.add(v.mul(tw[(r * at * unit) % N_FFT]))
                    });
            }
        }
    }
    step(x, 0, 1, N_FFT, tw, out);
}

/// Hz on the Slaney mel scale: linear to 1 kHz, logarithmic above.
fn hz_to_mel(hz: f64) -> f64 {
    let (min_log_hz, min_log_mel, step) = (1000.0, 15.0, 27.0 / 6.4f64.ln());
    if hz >= min_log_hz {
        min_log_mel + (hz / min_log_hz).ln() * step
    } else {
        3.0 * hz / 200.0
    }
}

fn mel_to_hz(mel: f64) -> f64 {
    let (min_log_hz, min_log_mel, step) = (1000.0, 15.0, 6.4f64.ln() / 27.0);
    if mel >= min_log_mel {
        min_log_hz * ((mel - min_log_mel) * step).exp()
    } else {
        200.0 * mel / 3.0
    }
}

/// One mel filter: the weights of the bins it spans, from `first` on.
struct Filter {
    first: usize,
    weights: Vec<f64>,
}

/// The mel filters, band by band: triangles evenly spaced in mel from 0 Hz
/// to 8 kHz, each scaled to the same area (Slaney); each kept as the bins
/// it spans, the few of the [`BINS`] that are not nought.
fn mel_filters() -> Vec<Filter> {
    let dense = dense_filters();
    (0..BANDS)
        .map(|m| {
            let weight = |k: usize| dense[k * BANDS + m];
            let first = (0..BINS).find(|&k| weight(k) != 0.0).unwrap_or(0);
            let last = (0..BINS).rev().find(|&k| weight(k) != 0.0).unwrap_or(0);
            Filter {
                first,
                weights: (first..=last.max(first)).map(weight).collect(),
            }
        })
        .collect()
}

/// [`mel_filters`] as a matrix, bin by band ([`BINS`] × [`BANDS`]).
fn dense_filters() -> Vec<f64> {
    let (lo, hi) = (hz_to_mel(0.0), hz_to_mel(RATE as f64 / 2.0));
    let edges: Vec<f64> = (0..BANDS + 2)
        .map(|i| mel_to_hz(lo + (hi - lo) * i as f64 / (BANDS + 1) as f64))
        .collect();
    let mut filters = vec![0f64; BINS * BANDS];
    for k in 0..BINS {
        let hz = (RATE / 2) as f64 * k as f64 / (BINS - 1) as f64;
        for m in 0..BANDS {
            let down = (hz - edges[m]) / (edges[m + 1] - edges[m]);
            let up = (edges[m + 2] - hz) / (edges[m + 2] - edges[m + 1]);
            let area = 2.0 / (edges[m + 2] - edges[m]);
            filters[k * BANDS + m] = down.min(up).max(0.0) * area;
        }
    }
    filters
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Eight seconds make 800 columns of 80 bands, short audio included:
    /// it is heard after silence.
    #[test]
    fn eight_seconds_make_eight_hundred_columns() {
        assert_eq!(frames(8), 800);
        assert_eq!(features(&[], 8).len(), BANDS * 800);
        assert_eq!(features(&vec![0.1; 100], 8).len(), BANDS * 800);
        assert_eq!(features(&vec![0.1; 20 * RATE], 8).len(), BANDS * 800);
    }

    /// What Hugging Face's extractor makes of a known signal — a sweep
    /// from 100 Hz to 7 kHz over 1.5 s under a 440 Hz tone, after 6.5 s of
    /// silence — at the loudest band of frames along the sweep and beside
    /// it, at the edges, and in the silence: the values below are its own,
    /// from `transformers` 5.22.
    #[test]
    fn the_bands_are_whispers() {
        use std::f64::consts::PI;
        let sweep: Vec<f32> = (0..RATE * 3 / 2)
            .map(|i| {
                let t = i as f64 / RATE as f64;
                let phase = 2.0 * PI * (100.0 * t + (7000.0 - 100.0) / 3.0 * t * t);
                (0.3 * phase.sin() + 0.1 * (2.0 * PI * 440.0 * t).sin()) as f32
            })
            .collect();
        let f = features(&sweep, 8);
        let at = |band: usize, frame: usize| f[band * 800 + frame];
        for (band, frame, want) in REFERENCE {
            let got = at(*band, *frame);
            assert!(
                (got - want).abs() < 2e-4,
                "band {band}, frame {frame}: {got}, not {want}"
            );
        }
    }

    /// The transform is the sum it stands for, bin for bin.
    #[test]
    fn the_transform_is_the_sum() {
        let mut x = [0f64; N_FFT];
        for (n, v) in x.iter_mut().enumerate() {
            *v = ((n * 7919) % 1000) as f64 / 500.0 - 1.0 + (n as f64 / 13.0).sin();
        }
        let tw = twiddles();
        let mut out = [Complex::default(); N_FFT];
        fft(&x, &tw, &mut out);
        for (k, got) in out.iter().enumerate() {
            let want = x
                .iter()
                .enumerate()
                .fold(Complex::default(), |sum, (n, &v)| {
                    sum.add(Complex { re: v, im: 0.0 }.mul(tw[(k * n) % N_FFT]))
                });
            assert!(
                (got.re - want.re).abs() < 1e-9 && (got.im - want.im).abs() < 1e-9,
                "bin {k}: {got:?}, not {want:?}"
            );
        }
    }

    /// Each filter kept as its span holds every weight the matrix does.
    #[test]
    fn the_filters_keep_every_weight() {
        let dense = dense_filters();
        let sparse = mel_filters();
        for (m, filter) in sparse.iter().enumerate() {
            for k in 0..BINS {
                let kept = k
                    .checked_sub(filter.first)
                    .and_then(|i| filter.weights.get(i))
                    .copied()
                    .unwrap_or(0.0);
                assert_eq!(kept, dense[k * BANDS + m], "band {m}, bin {k}");
            }
        }
    }

    /// Hugging Face's values for [`the_bands_are_whispers`]'s signal.
    #[allow(clippy::excessive_precision)]
    const REFERENCE: &[(usize, usize, f32)] = &[
        (0, 0, -0.009522795677185059),
        (79, 0, -0.16369175910949707),
        (0, 640, -0.009522795677185059),
        (40, 649, 0.027658700942993164),
        (40, 651, 0.022936105728149414),
        (79, 799, 0.7027406692504883),
        (12, 799, 1.4555537700653076),
        (8, 655, 1.8060412406921387),
        (6, 655, 1.5770492553710938),
        (11, 655, 1.587517499923706),
        (26, 670, 1.803757667541504),
        (24, 670, 1.429697036743164),
        (29, 670, 1.232092261314392),
        (43, 690, 1.7794904708862305),
        (41, 690, 1.085988998413086),
        (46, 690, 0.6242740154266357),
        (51, 705, 1.7603166103363037),
        (49, 705, 0.8577022552490234),
        (54, 705, 0.317352831363678),
        (57, 720, 1.740699052810669),
        (55, 720, 0.5592553615570068),
        (60, 720, 0.05232119560241699),
        (62, 735, 1.7268028259277344),
        (60, 735, 0.48636937141418457),
        (65, 735, -0.017092227935791016),
        (66, 750, 1.7064964771270752),
        (64, 750, 0.30372583866119385),
        (69, 750, -0.027912378311157227),
        (70, 765, 1.6729745864868164),
        (68, 765, 0.4280964136123657),
        (73, 765, -0.16369175910949707),
        (73, 780, 1.6781004667282104),
        (71, 780, 0.1444295048713684),
        (76, 780, -0.16369175910949707),
        (76, 795, 1.637939214706421),
        (74, 795, 0.29368096590042114),
        (79, 795, -0.16369175910949707),
    ];
}
