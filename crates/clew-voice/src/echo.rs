//! Echo cancellation: what Clew says aloud, taken back out of what the
//! microphone hears — so that the reader can talk over Clew, as over a
//! person, without the microphone taking Clew's own voice for theirs.
//!
//! The speaker's stream hands what it is about to play to an [`EchoBus`]
//! ([`EchoBus::played`]); the microphone's stream hands what it heard to a
//! [`Canceller`] on the same bus, which gives it back with the echo of what
//! was played removed: WebRTC's acoustic echo canceller (AEC3), in Rust
//! ([`sonora`]). Both streams keep their devices' own rates; the canceller
//! converts between them itself.
//!
//! Played sound is kept only while a canceller listens, and only the last
//! of it: nothing is gathered while the microphone is off.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use sonora::config::EchoCanceller;
use sonora::{AudioProcessing, Config, StreamConfig};

/// How much of what was played is kept, at most, for a canceller to read:
/// more than any device holds back between being given a sound and
/// playing it.
const KEEP_SECONDS: u32 = 2;

/// The delay, in milliseconds, assumed between a sound given to the
/// speaker and its echo reaching the canceller when a device does not
/// say: about what laptops' built-in speaker and microphone add together.
const DEFAULT_DELAY_MS: i32 = 60;

/// What the speaker played lately, for the microphone's echo to be taken
/// out of what it heard.
pub struct EchoBus {
    played: Mutex<Played>,
    /// Cancellers listening: what is played is kept only while one is.
    listening: AtomicUsize,
    /// How far ahead of being heard the speaker is given a sound, in
    /// milliseconds; negative while unknown.
    output_latency_ms: AtomicI32,
}

/// Sound given to the speaker and not yet read by a canceller: one
/// channel, at the speaker's rate.
struct Played {
    samples: VecDeque<f32>,
    rate: u32,
}

static BUS: LazyLock<Arc<EchoBus>> = LazyLock::new(|| Arc::new(EchoBus::new()));

/// The bus the system's speaker and microphone share.
pub fn bus() -> Arc<EchoBus> {
    BUS.clone()
}

impl EchoBus {
    /// A bus of its own (the system's is [`bus`]).
    pub fn new() -> EchoBus {
        EchoBus {
            played: Mutex::new(Played {
                samples: VecDeque::new(),
                rate: 0,
            }),
            listening: AtomicUsize::new(0),
            output_latency_ms: AtomicI32::new(-1),
        }
    }

    /// `samples` (one channel, at `rate` Hz) are given to the speaker now,
    /// to be heard `latency` from now when the device says how long.
    /// Silence counts: the canceller follows the speaker's clock by it.
    pub fn played(&self, samples: &[f32], rate: u32, latency: Option<Duration>) {
        if self.listening.load(Ordering::Relaxed) == 0 || rate == 0 {
            return;
        }
        if let Some(latency) = latency {
            let ms = i32::try_from(latency.as_millis()).unwrap_or(i32::MAX);
            self.output_latency_ms.store(ms, Ordering::Relaxed);
        }
        let Ok(mut played) = self.played.lock() else {
            return;
        };
        if played.rate != rate {
            played.samples.clear();
            played.rate = rate;
        }
        played.samples.extend(samples);
        let keep = (rate * KEEP_SECONDS) as usize;
        let over = played.samples.len().saturating_sub(keep);
        played.samples.drain(..over);
    }

    /// What was played since last asked, and its rate.
    fn take(&self) -> (Vec<f32>, u32) {
        match self.played.lock() {
            Ok(mut played) => (played.samples.drain(..).collect(), played.rate),
            Err(_) => (Vec::new(), 0),
        }
    }

    fn output_latency(&self) -> Option<i32> {
        let ms = self.output_latency_ms.load(Ordering::Relaxed);
        (ms >= 0).then_some(ms)
    }
}

impl Default for EchoBus {
    fn default() -> Self {
        EchoBus::new()
    }
}

/// What the microphone hears, with the echo of what the speaker played on
/// its [`EchoBus`] taken out of it.
pub struct Canceller {
    apm: AudioProcessing,
    bus: Arc<EchoBus>,
    /// The microphone's rate.
    rate: u32,
    /// What was heard, short of a whole 10 ms frame.
    heard: Vec<f32>,
    /// What was played, short of a whole 10 ms frame, and its rate.
    played: Vec<f32>,
    played_rate: u32,
}

impl Canceller {
    /// A canceller for a microphone at `rate` Hz, one channel, listening
    /// to what is played on `bus`.
    pub fn new(rate: u32, bus: Arc<EchoBus>) -> Canceller {
        let config = Config {
            echo_canceller: Some(EchoCanceller::default()),
            ..Default::default()
        };
        let apm = AudioProcessing::builder()
            .config(config)
            .capture_config(StreamConfig::new(rate, 1))
            .render_config(StreamConfig::new(rate, 1))
            .build();
        bus.listening.fetch_add(1, Ordering::Relaxed);
        Canceller {
            apm,
            bus,
            rate,
            heard: Vec::new(),
            played: Vec::new(),
            played_rate: 0,
        }
    }

    /// `heard` (one channel, at the microphone's rate), `latency` after it
    /// sounded when the device says: what is left of it once the echo of
    /// what was played is taken out — as much of it as makes whole 10 ms
    /// frames; the rest waits for what is heard next.
    pub fn clean(&mut self, heard: &[f32], latency: Option<Duration>) -> Vec<f32> {
        // What was played since, first: the echo in what was heard is of it.
        let (played, rate) = self.bus.take();
        if rate != self.played_rate {
            self.played.clear();
            self.played_rate = rate;
        }
        self.played.extend(played);
        if self.played_rate > 0 {
            let config = StreamConfig::new(self.played_rate, 1);
            let n = frame(self.played_rate);
            let mut out = vec![0.0f32; n];
            let mut at = 0;
            while self.played.len() - at >= n {
                let frame = &self.played[at..at + n];
                let _ = self.apm.process_render_f32_with_config(
                    &[frame],
                    &config,
                    &config,
                    &mut [&mut out],
                );
                at += n;
            }
            self.played.drain(..at);
        }
        let input_ms = latency.map_or(0, |l| i32::try_from(l.as_millis()).unwrap_or(0));
        let delay = self
            .bus
            .output_latency()
            .map_or(DEFAULT_DELAY_MS, |out| out + input_ms);
        let _ = self.apm.set_stream_delay_ms(delay.clamp(0, 500));
        self.heard.extend_from_slice(heard);
        let n = frame(self.rate);
        let mut cleaned = Vec::with_capacity(self.heard.len());
        let mut out = vec![0.0f32; n];
        let mut at = 0;
        while self.heard.len() - at >= n {
            let frame = &self.heard[at..at + n];
            match self.apm.process_capture_f32(&[frame], &mut [&mut out]) {
                Ok(()) => cleaned.extend_from_slice(&out),
                // Never lose what was said: as heard, then.
                Err(_) => cleaned.extend_from_slice(frame),
            }
            at += n;
        }
        self.heard.drain(..at);
        cleaned
    }

    /// How much the echo is reduced by, in dB, once the canceller has
    /// measured it.
    pub fn reduction_db(&self) -> Option<f64> {
        self.apm.statistics().echo_return_loss_enhancement
    }
}

impl Drop for Canceller {
    fn drop(&mut self) {
        self.bus.listening.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One 10 ms frame at `rate` Hz, as the canceller takes its sound.
fn frame(rate: u32) -> usize {
    (rate / 100).max(1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::rms;

    /// Speech-like sound: bursts of noise shaped by a few tones, as
    /// syllables come and go — what the canceller is made for, unlike a
    /// steady tone.
    fn speech_like(seed: u32, seconds: f32, rate: u32) -> Vec<f32> {
        let n = (seconds * rate as f32) as usize;
        let mut state = seed.max(1);
        let mut noise = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        let f = [180.0 + seed as f32 * 7.0, 410.0, 930.0 + seed as f32 * 13.0];
        (0..n)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let syllable = (t * 4.0 * std::f32::consts::TAU).sin().max(0.0);
                let voice: f32 = f
                    .iter()
                    .map(|f| (t * f * std::f32::consts::TAU).sin())
                    .sum::<f32>()
                    / 3.0;
                0.3 * syllable * (0.6 * voice + 0.4 * noise())
            })
            .collect()
    }

    /// The echo of `played`, as a room returns it: `delay_ms` later,
    /// quieter.
    fn echo_of(played: &[f32], rate: u32, delay_ms: usize, gain: f32) -> Vec<f32> {
        let delay = rate as usize * delay_ms / 1000;
        (0..played.len())
            .map(|i| i.checked_sub(delay).map_or(0.0, |j| played[j] * gain))
            .collect()
    }

    /// Run `played` and `heard` through a canceller as the two streams
    /// would, 10 ms at a time, returning what it gives back.
    fn run(played: &[f32], heard: &[f32], rate: u32) -> Vec<f32> {
        let bus = Arc::new(EchoBus::new());
        let mut canceller = Canceller::new(rate, bus.clone());
        let n = frame(rate);
        let mut cleaned = Vec::new();
        for (p, h) in played.chunks(n).zip(heard.chunks(n)) {
            bus.played(p, rate, Some(Duration::from_millis(20)));
            cleaned.extend(canceller.clean(h, Some(Duration::from_millis(20))));
        }
        cleaned
    }

    /// Clew alone, heard back by the microphone: once the canceller has
    /// found the echo, little of it is left.
    #[test]
    fn the_echo_of_what_is_played_is_taken_out() {
        let rate = 16_000;
        let played = speech_like(1, 6.0, rate);
        let heard = echo_of(&played, rate, 40, 0.5);
        let cleaned = run(&played, &heard, rate);
        assert_eq!(cleaned.len(), heard.len());
        let tail = |s: &[f32]| rms(&s[s.len() / 2..]);
        let (before, after) = (tail(&heard), tail(&cleaned));
        assert!(
            after < before / 4.0,
            "the echo is 12 dB down once found: {before} → {after}"
        );
    }

    /// The reader talking over Clew is kept: what is left is their voice.
    #[test]
    fn a_voice_over_the_echo_is_kept() {
        let rate = 16_000;
        let played = speech_like(1, 6.0, rate);
        let voice = speech_like(9, 6.0, rate);
        let echo = echo_of(&played, rate, 40, 0.5);
        let heard: Vec<f32> = echo.iter().zip(&voice).map(|(e, v)| e + v).collect();
        let cleaned = run(&played, &heard, rate);
        let tail = |s: &[f32]| rms(&s[s.len() / 2..]);
        assert!(
            tail(&cleaned) > tail(&voice) / 2.0,
            "the voice is kept: {} of {}",
            tail(&cleaned),
            tail(&voice)
        );
    }

    /// The speaker and the microphone at rates of their own — 48 kHz out,
    /// 16 kHz in, as a laptop's often are: the echo is found all the same.
    #[test]
    fn the_echo_is_found_across_rates() {
        let (out_rate, in_rate) = (48_000, 16_000);
        let played = speech_like(1, 6.0, out_rate);
        let heard = echo_of(
            &crate::audio::resample(&played, out_rate, in_rate),
            in_rate,
            40,
            0.5,
        );
        let bus = Arc::new(EchoBus::new());
        let mut canceller = Canceller::new(in_rate, bus.clone());
        let mut cleaned = Vec::new();
        for (p, h) in played
            .chunks(frame(out_rate))
            .zip(heard.chunks(frame(in_rate)))
        {
            bus.played(p, out_rate, Some(Duration::from_millis(20)));
            cleaned.extend(canceller.clean(h, Some(Duration::from_millis(20))));
        }
        let tail = |s: &[f32]| rms(&s[s.len() / 2..]);
        let (before, after) = (tail(&heard), tail(&cleaned));
        assert!(after < before / 4.0, "{before} → {after}");
    }

    /// Nothing played — the microphone on with Clew quiet — what is heard
    /// is given back as it was, in step.
    #[test]
    fn with_nothing_played_what_is_heard_passes() {
        let rate = 48_000;
        let voice = speech_like(3, 2.0, rate);
        let silence = vec![0.0; voice.len()];
        let cleaned = run(&silence, &voice, rate);
        assert_eq!(cleaned.len(), voice.len());
        let tail = |s: &[f32]| rms(&s[s.len() / 2..]);
        assert!(
            tail(&cleaned) > tail(&voice) * 0.7,
            "{} of {}",
            tail(&cleaned),
            tail(&voice)
        );
    }

    /// Through real devices — the system's speaker, and a microphone that
    /// hears it, as a laptop's does (a loopback between them, such as
    /// PulseAudio's null sinks: the speaker `spk`, the reader `user`, both
    /// looped into the room the default microphone hears): Clew speaks for
    /// eight seconds and the reader talks over it from the fifth to the
    /// sixth and a half. Heard with the canceller, Clew's echo is all but
    /// gone and the reader's voice is not.
    #[test]
    #[ignore = "needs a loopback from the speaker to the microphone: CLEW_TEST_LOOPBACK=1"]
    fn the_echo_is_taken_out_through_real_devices() {
        use std::io::Write;
        if std::env::var_os("CLEW_TEST_LOOPBACK").is_none() {
            return;
        }
        let run = |cancel: bool| -> (f32, f32) {
            let recorder = crate::capture::Recorder::start(30, cancel).expect("a microphone");
            let player = crate::playback::Player::start().expect("a speaker");
            let rate = player.rate();
            player.play(&speech_like(1, 8.0, rate), rate);
            std::thread::sleep(Duration::from_millis(5000));
            // The reader, from another source in the room.
            let voice = speech_like(9, 1.5, 48_000);
            let bytes: Vec<u8> = voice.iter().flat_map(|s| s.to_le_bytes()).collect();
            let mut reader = std::process::Command::new("pacat")
                .args([
                    "--playback",
                    "--device=user",
                    "--format=float32le",
                    "--rate=48000",
                    "--channels=1",
                ])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("pacat");
            reader.stdin.take().unwrap().write_all(&bytes).unwrap();
            let _ = reader.wait();
            std::thread::sleep(Duration::from_millis(2500));
            let heard = recorder.stop();
            drop(player);
            let at = |seconds: f32| (seconds * heard.rate as f32) as usize;
            let window =
                |from: f32, to: f32| rms(&heard.samples[at(from)..at(to).min(heard.samples.len())]);
            // Clew alone; then Clew and the reader.
            (window(2.5, 4.5), window(5.4, 6.3))
        };
        let (raw_echo, raw_both) = run(false);
        let (echo, both) = run(true);
        eprintln!(
            "without: echo {raw_echo:.4}, with the reader {raw_both:.4}; with: echo {echo:.4}, with the reader {both:.4}"
        );
        assert!(raw_echo > 0.01, "the microphone heard Clew: {raw_echo}");
        assert!(
            echo < raw_echo / 4.0,
            "the echo is 12 dB down: {raw_echo} → {echo}"
        );
        assert!(
            both > echo * 3.0,
            "the reader is heard over what is left: {echo} → {both}"
        );
    }

    /// What is played is kept only while a canceller listens, and not
    /// more than a couple of seconds of it.
    #[test]
    fn the_bus_keeps_only_what_a_canceller_will_read() {
        let bus = Arc::new(EchoBus::new());
        bus.played(&[0.5; 480], 48_000, None);
        assert!(bus.take().0.is_empty(), "nobody listening: nothing kept");
        let canceller = Canceller::new(48_000, bus.clone());
        bus.played(&vec![0.5; 48_000 * 5], 48_000, None);
        let (kept, rate) = bus.take();
        assert_eq!(rate, 48_000);
        assert_eq!(kept.len(), 48_000 * KEEP_SECONDS as usize);
        drop(canceller);
        bus.played(&[0.5; 480], 48_000, None);
        assert!(bus.take().0.is_empty());
    }
}
