//! Sound played on the system's default output, through cpal: what Clew
//! says aloud ([`crate::speech`]).
//!
//! A [`Player`] owns a thread that owns the output stream — a stream is not
//! `Send` on every platform — and plays what it is given, one sentence after
//! another, resampled to the device's own rate ([`crate::audio::resample`]),
//! until it is cleared or dropped. It can be paused — while the reader
//! talks over Clew — and resumed where it was, fading out and back in.
//!
//! What it plays, silence too, goes to the [`echo`](crate::echo) bus, so
//! that the microphone's echo of it can be taken out of what it hears.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use crate::audio;

/// What the stream's callback and the player share.
struct Shared {
    /// What is still to be played, at the device's rate, one channel.
    queue: Mutex<VecDeque<f32>>,
    /// Samples played since the player started.
    played: AtomicU64,
    /// The latest buffer's level ([`audio::meter`]), as `f32` bits.
    level: AtomicU32,
    /// A failure the stream reported after it started.
    failed: Mutex<Option<String>>,
    /// Held: nothing more is played until it is resumed.
    paused: AtomicBool,
}

/// How long a pause fades out over, and a resume back in.
const FADE_SECONDS: f32 = 0.01;

/// The default output, playing.
pub struct Player {
    shared: Arc<Shared>,
    rate: u32,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

/// How long opening the output may take before it is given up on.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

impl Player {
    /// Open the default output.
    pub fn start() -> Result<Player, String> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, Arc<Shared>), String>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("clew-speaker".into())
            .spawn(move || {
                let stream = match open() {
                    Ok((stream, rate, shared)) => {
                        let _ = ready_tx.send(Ok((rate, shared)));
                        stream
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = stop_rx.recv();
                let _ = stream.pause();
                drop(stream);
            })
            .map_err(|e| format!("could not start the speaker: {e}"))?;
        match ready_rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(Ok((rate, shared))) => Ok(Player {
                shared,
                rate,
                stop: Some(stop_tx),
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                drop(stop_tx);
                Err("the speaker did not open in time".into())
            }
        }
    }

    /// The device's rate, in Hz.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Play `samples` (one channel, at `rate` Hz) after what is queued.
    pub fn play(&self, samples: &[f32], rate: u32) {
        let samples = audio::resample(samples, rate, self.rate);
        if let Ok(mut queue) = self.shared.queue.lock() {
            queue.extend(samples);
        }
    }

    /// Hold what is playing, fading it out: what is queued waits, to be
    /// played from where it was held ([`Player::resume`]).
    pub fn pause(&self) {
        self.shared.paused.store(true, Ordering::Relaxed);
    }

    /// Go on playing what was held, fading back in.
    pub fn resume(&self) {
        self.shared.paused.store(false, Ordering::Relaxed);
    }

    /// Whether it is held.
    pub fn paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    /// Stop: what is queued is not played.
    pub fn clear(&self) {
        if let Ok(mut queue) = self.shared.queue.lock() {
            queue.clear();
        }
        self.shared.level.store(0, Ordering::Relaxed);
    }

    /// Samples played since it started: what tells a stream that plays
    /// from one that has stopped.
    pub fn played(&self) -> u64 {
        self.shared.played.load(Ordering::Relaxed)
    }

    /// What is still to be played, taken from it — to be played elsewhere —
    /// and its rate.
    pub fn take_rest(&self) -> (Vec<f32>, u32) {
        let rest = self
            .shared
            .queue
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default();
        (rest, self.rate)
    }

    /// Seconds still to play.
    pub fn queued_seconds(&self) -> f32 {
        let n = self.shared.queue.lock().map_or(0, |q| q.len());
        n as f32 / self.rate.max(1) as f32
    }

    /// Whether nothing is left to play.
    pub fn idle(&self) -> bool {
        self.shared.queue.lock().is_ok_and(|q| q.is_empty())
    }

    /// How loud what is playing is, 0 to 1.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::Relaxed))
    }

    /// A failure since it started, such as the device going away.
    pub fn error(&self) -> Option<String> {
        self.shared.failed.lock().ok().and_then(|f| f.clone())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Open the default output and start it, playing from a fresh [`Shared`].
fn open() -> Result<(cpal::Stream, u32, Arc<Shared>), String> {
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        return Err("no speaker or headphones are available".into());
    };
    let supported = device
        .default_output_config()
        .map_err(|e| format!("the speaker failed: {e}"))?;
    let rate = supported.sample_rate();
    let channels = usize::from(supported.channels()).max(1);
    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::new()),
        played: AtomicU64::new(0),
        level: AtomicU32::new(0),
        failed: Mutex::new(None),
        paused: AtomicBool::new(false),
    });
    let config = supported.config();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, config, channels, &shared),
        SampleFormat::F64 => build::<f64>(&device, config, channels, &shared),
        SampleFormat::I16 => build::<i16>(&device, config, channels, &shared),
        SampleFormat::I32 => build::<i32>(&device, config, channels, &shared),
        SampleFormat::U16 => build::<u16>(&device, config, channels, &shared),
        SampleFormat::U8 => build::<u8>(&device, config, channels, &shared),
        other => {
            return Err(format!(
                "the speaker's sample format ({other}) is not supported"
            ));
        }
    }
    .map_err(|e| format!("the speaker failed: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("the speaker failed: {e}"))?;
    Ok((stream, rate, shared))
}

/// The next `frames` samples to play, taken from `queue`: let through at
/// `gain`, moved toward `target` by `step` a sample — faded out to a pause
/// (0), back in from one (1). Paused, what is queued waits. Also how many
/// were taken.
fn fill(
    queue: &mut VecDeque<f32>,
    frames: usize,
    target: f32,
    gain: &mut f32,
    step: f32,
) -> (Vec<f32>, u64) {
    let mut out = Vec::with_capacity(frames);
    let mut taken = 0u64;
    for _ in 0..frames {
        if target == 0.0 && *gain <= 0.0 {
            // Held: what is queued waits.
            out.push(0.0);
            continue;
        }
        *gain = if target > *gain {
            (*gain + step).min(target)
        } else {
            (*gain - step).max(target)
        };
        match queue.pop_front() {
            Some(sample) => {
                taken += 1;
                out.push(sample * *gain);
            }
            None => out.push(0.0),
        }
    }
    (out, taken)
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    shared: &Arc<Shared>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let data = shared.clone();
    let errors = shared.clone();
    let rate = config.sample_rate;
    let echo = crate::echo::bus();
    // How loud what is played is let through: falling to nothing as it is
    // paused, rising back as it resumes.
    let step = 1.0 / (rate as f32 * FADE_SECONDS).max(1.0);
    let mut gain = 1.0f32;
    device.build_output_stream::<T, _, _>(
        config,
        move |output: &mut [T], info: &cpal::OutputCallbackInfo| {
            let frames = output.len() / channels;
            let target = if data.paused.load(Ordering::Relaxed) {
                0.0
            } else {
                1.0
            };
            let (mut mono, drained) = match data.queue.lock() {
                Ok(mut queue) => fill(&mut queue, frames, target, &mut gain, step),
                Err(_) => (vec![0.0; frames], 0),
            };
            data.played.fetch_add(drained, Ordering::Relaxed);
            data.level
                .store(audio::meter(audio::rms(&mono)).to_bits(), Ordering::Relaxed);
            mono.resize(frames, 0.0);
            let timestamp = info.timestamp();
            echo.played(
                &mono,
                rate,
                Some(timestamp.playback.duration_since(timestamp.callback)),
            );
            for (frame, sample) in output.chunks_mut(channels).zip(&mono) {
                for out in frame {
                    *out = T::from_sample(*sample);
                }
            }
        },
        move |e| {
            if let Ok(mut failed) = errors.failed.lock() {
                failed.get_or_insert_with(|| format!("the speaker failed: {e}"));
            }
        },
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paused, what is playing fades out and then waits; resumed, it fades
    /// back in and goes on from where it waited — nothing skipped but the
    /// fade.
    #[test]
    fn a_pause_fades_out_holds_and_goes_on() {
        let mut queue: VecDeque<f32> = (0..1000).map(|i| 1.0 + i as f32).collect();
        let step = 0.25;
        let mut gain = 1.0;
        let (out, taken) = fill(&mut queue, 4, 1.0, &mut gain, step);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(taken, 4);
        // Paused: four samples fading out, then silence, the queue held.
        let (out, taken) = fill(&mut queue, 10, 0.0, &mut gain, step);
        assert_eq!(&out[..4], &[5.0 * 0.75, 6.0 * 0.5, 7.0 * 0.25, 0.0]);
        assert!(out[4..].iter().all(|s| *s == 0.0));
        assert_eq!(taken, 4);
        let (out, taken) = fill(&mut queue, 10, 0.0, &mut gain, step);
        assert!(out.iter().all(|s| *s == 0.0) && taken == 0, "held");
        // Resumed: from where it was held, fading back in.
        let (out, _) = fill(&mut queue, 6, 1.0, &mut gain, step);
        assert_eq!(out, [9.0 * 0.25, 10.0 * 0.5, 11.0 * 0.75, 12.0, 13.0, 14.0]);
        // Nothing left: silence, nothing taken.
        let mut empty = VecDeque::new();
        let (out, taken) = fill(&mut empty, 3, 1.0, &mut gain, step);
        assert_eq!((out, taken), (vec![0.0; 3], 0));
    }

    /// The default output plays what it is given and says when all of it
    /// has been played — when the test is pointed at one.
    #[test]
    #[ignore = "needs an output device: CLEW_TEST_SPEAKER=1"]
    fn a_real_output_plays() {
        let player = Player::start().expect("the output opens");
        let rate = 44_100;
        let tone: Vec<f32> = (0..rate)
            .map(|i| 0.3 * (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin())
            .collect();
        player.play(&tone, rate as u32);
        assert!(!player.idle());
        let mut heard = 0.0f32;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            heard = heard.max(player.level());
            eprintln!(
                "idle={} queued={:.2} level={:.2} error={:?}",
                player.idle(),
                player.queued_seconds(),
                player.level(),
                player.error()
            );
            if player.idle() {
                break;
            }
        }
        assert!(player.idle(), "all of it played");
        assert!(player.played() >= u64::from(player.rate()) * 9 / 10);
        assert!(heard > 0.1, "it was loud: {heard}");
        assert_eq!(player.error(), None);
    }
}
