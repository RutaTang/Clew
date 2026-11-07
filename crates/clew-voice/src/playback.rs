//! Sound played on the system's default output, through cpal: what Clew
//! says aloud ([`crate::speech`]).
//!
//! A [`Player`] owns a thread that owns the output stream — a stream is not
//! `Send` on every platform — and plays what it is given, one sentence after
//! another, resampled to the device's own rate ([`crate::audio::resample`]),
//! until it is cleared or dropped.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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
}

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

    /// Stop: what is queued is not played.
    pub fn clear(&self) {
        if let Ok(mut queue) = self.shared.queue.lock() {
            queue.clear();
        }
        self.shared.level.store(0, Ordering::Relaxed);
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
    let device = host
        .default_output_device()
        .ok_or("no speaker or headphones are available")?;
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
    device.build_output_stream::<T, _, _>(
        config,
        move |output: &mut [T], _| {
            let frames = output.len() / channels;
            let mut mono = Vec::with_capacity(frames);
            if let Ok(mut queue) = data.queue.lock() {
                let n = frames.min(queue.len());
                mono.extend(queue.drain(..n));
            }
            data.played.fetch_add(mono.len() as u64, Ordering::Relaxed);
            data.level
                .store(audio::meter(audio::rms(&mono)).to_bits(), Ordering::Relaxed);
            mono.resize(frames, 0.0);
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
