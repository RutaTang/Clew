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
    let (cos, sin) = twiddles();
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
    let mut power = [0f64; BINS];
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
                for (k, p) in power.iter_mut().enumerate() {
                    let (mut re, mut im) = (0f64, 0f64);
                    for (n, &b) in buffer.iter().enumerate() {
                        let t = (k * n) % N_FFT;
                        re += b * cos[t];
                        im -= b * sin[t];
                    }
                    *p = re * re + im * im;
                }
                let mut bands = [0f64; BANDS];
                for (m, band) in bands.iter_mut().enumerate() {
                    let energy: f64 = power
                        .iter()
                        .enumerate()
                        .map(|(k, p)| p * filters[k * BANDS + m])
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

/// cos and sin of 2πt/N for every t: a DFT of the window's bins reads them.
fn twiddles() -> ([f64; N_FFT], [f64; N_FFT]) {
    let mut cos = [0f64; N_FFT];
    let mut sin = [0f64; N_FFT];
    for t in 0..N_FFT {
        let a = 2.0 * std::f64::consts::PI * t as f64 / N_FFT as f64;
        cos[t] = a.cos();
        sin[t] = a.sin();
    }
    (cos, sin)
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

/// The mel filters, bin by band ([`BINS`] × [`BANDS`]): triangles evenly
/// spaced in mel from 0 Hz to 8 kHz, each scaled to the same area (Slaney).
fn mel_filters() -> Vec<f64> {
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
