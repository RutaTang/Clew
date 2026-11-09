//! Listening to the microphone: the system's default input, through cpal
//! (Core Audio on macOS, WASAPI on Windows, ALSA on Linux — which PipeWire
//! and PulseAudio both serve).
//!
//! A [`Recorder`] owns a thread that owns the input stream — a stream is
//! not `Send` on every platform — and gathers what it hears, one channel at
//! the device's own rate, until it is stopped or has heard as much as it
//! may keep — with what the speaker plays taken out of it when asked
//! ([`crate::echo`]): Clew's own voice is then not heard as the reader's.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample};

use crate::audio;

/// What a recording heard: one channel, at `rate` Hz.
#[derive(Debug, Clone, Default)]
pub struct Recording {
    pub samples: Vec<f32>,
    pub rate: u32,
}

impl Recording {
    /// Its length in seconds.
    pub fn seconds(&self) -> f32 {
        if self.rate == 0 {
            0.0
        } else {
            self.samples.len() as f32 / self.rate as f32
        }
    }
}

/// What the stream's callback and the recorder share.
struct Shared {
    samples: Mutex<Vec<f32>>,
    /// Most samples kept; past it the rest are dropped and `full` is set.
    max: usize,
    full: AtomicBool,
    /// The meter's reading for the latest buffer, as `f32` bits.
    level: AtomicU32,
    /// The level ([`audio::rms`]) of every [`audio::FRAME_SECONDS`] heard.
    levels: Mutex<Vec<f32>>,
    /// A failure the stream reported after it started (the device was
    /// unplugged).
    failed: Mutex<Option<String>>,
}

/// The microphone, listening.
pub struct Recorder {
    shared: Arc<Shared>,
    rate: u32,
    device: String,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

/// How long opening the microphone may take before it is given up on.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

impl Recorder {
    /// Start listening on the default input, keeping at most `max_seconds`
    /// — with the echo of what the speaker plays taken out of what is
    /// heard, when `cancel_echo`.
    pub fn start(max_seconds: u32, cancel_echo: bool) -> Result<Recorder, String> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, String, Arc<Shared>), String>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("clew-microphone".into())
            .spawn(move || {
                let stream = match open(max_seconds, cancel_echo) {
                    Ok((stream, rate, device, shared)) => {
                        let _ = ready_tx.send(Ok((rate, device, shared)));
                        stream
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                // Held until stopped (or the recorder is dropped, which
                // closes the channel).
                let _ = stop_rx.recv();
                let _ = stream.pause();
                drop(stream);
            })
            .map_err(|e| format!("could not start listening: {e}"))?;
        match ready_rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(Ok((rate, device, shared))) => Ok(Recorder {
                shared,
                rate,
                device,
                stop: Some(stop_tx),
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                // Left to finish on its own; the dropped sender stops it.
                drop(stop_tx);
                Err("the microphone did not open in time".into())
            }
        }
    }

    /// The input's name, as the system calls it.
    pub fn device(&self) -> &str {
        &self.device
    }

    /// How loud the latest moment was, 0 to 1 ([`audio::meter`]).
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::Relaxed))
    }

    /// Seconds heard so far.
    pub fn seconds(&self) -> f32 {
        let n = self.shared.samples.lock().map_or(0, |s| s.len());
        n as f32 / self.rate.max(1) as f32
    }

    /// How long it has been quiet since the speaker last spoke, in
    /// seconds; `None` until something sounded like speech
    /// ([`audio::trailing_silence`]).
    pub fn silence_after_speech(&self) -> Option<f32> {
        let levels = self.shared.levels.lock().ok()?;
        audio::trailing_silence(&levels)
    }

    /// The last `seconds` of what it has heard, and the rate they are at:
    /// what a model that hears the end of an utterance is given, while
    /// listening goes on. Copied as they are — the input's lock is held no
    /// longer — for the caller to resample elsewhere.
    pub fn recent(&self, seconds: f32) -> (Vec<f32>, u32) {
        let Ok(samples) = self.shared.samples.lock() else {
            return (Vec::new(), self.rate);
        };
        let keep = ((seconds.max(0.0) * self.rate as f32) as usize).min(samples.len());
        (samples[samples.len() - keep..].to_vec(), self.rate)
    }

    /// Whether it has heard all it may keep.
    pub fn full(&self) -> bool {
        self.shared.full.load(Ordering::Relaxed)
    }

    /// A failure since it started, such as the device going away.
    pub fn error(&self) -> Option<String> {
        self.shared.failed.lock().ok().and_then(|f| f.clone())
    }

    /// Stop listening and hand over what was heard.
    pub fn stop(mut self) -> Recording {
        self.close();
        let samples = self
            .shared
            .samples
            .lock()
            .map(|mut s| std::mem::take(&mut *s))
            .unwrap_or_default();
        Recording {
            samples,
            rate: self.rate,
        }
    }

    fn close(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.close();
    }
}

/// Open the default input and start it, gathering into a fresh [`Shared`].
fn open(
    max_seconds: u32,
    cancel_echo: bool,
) -> Result<(cpal::Stream, u32, String, Arc<Shared>), String> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or(NO_MICROPHONE)?;
    let name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "the default input".into());
    let supported = device.default_input_config().map_err(|e| failure(&e))?;
    let rate = supported.sample_rate();
    let channels = usize::from(supported.channels()).max(1);
    let shared = Arc::new(Shared {
        samples: Mutex::new(Vec::new()),
        max: rate as usize * max_seconds as usize,
        full: AtomicBool::new(false),
        level: AtomicU32::new(0),
        levels: Mutex::new(Vec::new()),
        failed: Mutex::new(None),
    });
    let config = supported.config();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, config, channels, &shared, cancel_echo),
        SampleFormat::F64 => build::<f64>(&device, config, channels, &shared, cancel_echo),
        SampleFormat::I8 => build::<i8>(&device, config, channels, &shared, cancel_echo),
        SampleFormat::I16 => build::<i16>(&device, config, channels, &shared, cancel_echo),
        SampleFormat::I32 => build::<i32>(&device, config, channels, &shared, cancel_echo),
        SampleFormat::U8 => build::<u8>(&device, config, channels, &shared, cancel_echo),
        SampleFormat::U16 => build::<u16>(&device, config, channels, &shared, cancel_echo),
        SampleFormat::U32 => build::<u32>(&device, config, channels, &shared, cancel_echo),
        other => {
            return Err(format!(
                "the microphone's sample format ({other}) is not supported"
            ));
        }
    }
    .map_err(|e| failure(&e))?;
    stream.play().map_err(|e| failure(&e))?;
    Ok((stream, rate, name, shared))
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    shared: &Arc<Shared>,
    cancel_echo: bool,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let data = shared.clone();
    let errors = shared.clone();
    let mut canceller =
        cancel_echo.then(|| crate::echo::Canceller::new(config.sample_rate, crate::echo::bus()));
    // One level frame's worth of samples at the device's rate, and the
    // frame being summed.
    let frame = ((config.sample_rate as f32 * audio::FRAME_SECONDS) as usize).max(1);
    let (mut sum, mut count) = (0.0f64, 0usize);
    device.build_input_stream::<T, _, _>(
        config,
        move |input: &[T], info: &cpal::InputCallbackInfo| {
            let floats: Vec<f32> = input.iter().map(|s| s.to_sample::<f32>()).collect();
            let mono = audio::mono(&floats, channels);
            // What the speaker played, taken out: from here on, what is
            // measured and kept is what was said.
            let mono = match canceller.as_mut() {
                Some(canceller) => {
                    let timestamp = info.timestamp();
                    canceller.clean(
                        &mono,
                        Some(timestamp.callback.duration_since(timestamp.capture)),
                    )
                }
                None => mono,
            };
            data.level
                .store(audio::meter(audio::rms(&mono)).to_bits(), Ordering::Relaxed);
            let mut done = Vec::new();
            for s in &mono {
                sum += f64::from(*s) * f64::from(*s);
                count += 1;
                if count == frame {
                    done.push((sum / count as f64).sqrt() as f32);
                    (sum, count) = (0.0, 0);
                }
            }
            if !done.is_empty()
                && let Ok(mut levels) = data.levels.lock()
            {
                levels.extend(done);
            }
            if let Ok(mut samples) = data.samples.lock() {
                let room = data.max.saturating_sub(samples.len());
                if mono.len() > room {
                    data.full.store(true, Ordering::Relaxed);
                }
                samples.extend_from_slice(&mono[..mono.len().min(room)]);
            }
        },
        move |e| {
            if let Ok(mut failed) = errors.failed.lock() {
                failed.get_or_insert_with(|| failure_while_listening(&e));
            }
        },
        None,
    )
}

/// A cpal error opening the microphone, as the window says it.
fn failure(e: &cpal::Error) -> String {
    match e.kind() {
        ErrorKind::PermissionDenied => permission_hint().to_string(),
        ErrorKind::DeviceNotAvailable => NO_MICROPHONE.to_string(),
        ErrorKind::DeviceBusy => "the microphone is busy in another app".to_string(),
        _ => format!("the microphone failed: {e}"),
    }
}

/// A cpal error while listening, as the window says it.
fn failure_while_listening(e: &cpal::Error) -> String {
    match e.kind() {
        ErrorKind::DeviceNotAvailable | ErrorKind::DeviceChanged => {
            "the microphone went away — it may have been unplugged".to_string()
        }
        _ => failure(e),
    }
}

/// What opening the microphone says when there is none to open.
const NO_MICROPHONE: &str =
    "no microphone is available — connect one, or pick an input in the system's sound settings";

/// Where the user allows Clew to use the microphone, on this system.
pub fn permission_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "Clew may not use the microphone — allow it in System Settings → Privacy & Security → Microphone"
    } else if cfg!(windows) {
        "Clew may not use the microphone — allow it in Settings → Privacy & security → Microphone"
    } else {
        "Clew may not use the microphone — check that your user may record audio"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recordings_length_is_in_seconds() {
        let r = Recording {
            samples: vec![0.0; 24_000],
            rate: 48_000,
        };
        assert_eq!(r.seconds(), 0.5);
        assert_eq!(Recording::default().seconds(), 0.0);
    }

    /// The failures the user may meet are said in words, with where to
    /// fix the one they can.
    #[test]
    fn microphone_failures_are_said_in_words() {
        let denied = failure(&cpal::Error::new(ErrorKind::PermissionDenied));
        assert_eq!(denied, permission_hint());
        assert!(denied.contains("Microphone") || denied.contains("record audio"));
        assert_eq!(
            failure(&cpal::Error::new(ErrorKind::DeviceNotAvailable)),
            NO_MICROPHONE
        );
        assert!(
            failure_while_listening(&cpal::Error::new(ErrorKind::DeviceNotAvailable))
                .contains("unplugged")
        );
        assert_eq!(
            failure_while_listening(&cpal::Error::new(ErrorKind::PermissionDenied)),
            permission_hint()
        );
        assert!(failure(&cpal::Error::new(ErrorKind::DeviceBusy)).contains("busy"));
        let other = failure(&cpal::Error::with_message(
            ErrorKind::InvalidInput,
            "no such stream",
        ));
        assert!(other.contains("no such stream"), "{other}");
    }

    /// The system's microphone, opened for real: where the machine has
    /// one, it listens; where it has none (a CI runner), it says so in
    /// words. Opt in with `CLEW_TEST_MICROPHONE=1`: on a desktop it is a
    /// real microphone (macOS asks the user first), and some audio stacks
    /// write their own state under HOME, which the hermetic test run
    /// forbids.
    #[test]
    fn the_microphone_opens_or_says_why_not() {
        if std::env::var("CLEW_TEST_MICROPHONE").as_deref() != Ok("1") {
            eprintln!("skipped: set CLEW_TEST_MICROPHONE=1 to open the microphone");
            return;
        }
        match Recorder::start(1, false) {
            Ok(recorder) => {
                assert!(!recorder.device().is_empty());
                std::thread::sleep(Duration::from_millis(200));
                let level = recorder.level();
                assert!((0.0..=1.0).contains(&level), "{level}");
                assert_eq!(recorder.error(), None);
                let recording = recorder.stop();
                assert!(recording.rate > 0);
                assert!(recording.seconds() <= 1.0);
            }
            Err(e) => {
                eprintln!("no microphone: {e}");
                assert!(!e.is_empty());
            }
        }
    }
}
