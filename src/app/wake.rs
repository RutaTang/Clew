//! Waiting to be called ([`clew_voice::wake`]): with "Start listening when
//! you say “Hey, Clew”" on, the microphone waits whenever it is not
//! listening, and "Hey, Clew" starts listening; "Bye, Clew"
//! ([`crate::app::spoken::stops_listening`]) — or a while with nothing said
//! ([`IDLE`]) — has it wait again. Both are said as they are in every
//! language.
//!
//! While it waits, what the microphone hears goes, piece by piece, to a
//! small keyword spotter (`clew-speak listen`) and nowhere else: nothing is
//! kept or worked out into words — but the last moment of it, so that what
//! is said in one breath with the phrase ("Hey, Clew, open Settings") is
//! heard whole. It is heard off the window's thread ([`subscription`]): the
//! window hears of it only when the phrase is said, or the waiting fails.
//! One window waits at a time.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use clew_voice::capture::Recording;
use clew_voice::wake::{self as spot, Feed, MODEL, Spotters};
use iced::{Subscription, Task};

use crate::app::voice::{
    Claim, ClaimedFor, Listening, MAX_SECONDS, ModelDownload, Pref, StopOnDrop, VoiceMsg,
};
use crate::*;

/// How much of what was heard last is kept: how far back a phrase heard
/// is looked for as speech listening hears ([`Wait::heard_as_speech`]),
/// and the lead of what is worked out once it is called — the call itself,
/// whose name proves it ([`App::prove_call`]), and what was said in one
/// breath with it. The spotter may say it heard the phrase as late as the
/// pieces queued for it, more than a second on a busy computer, and the
/// phrase must still be in it.
pub(crate) const HEARD_SECONDS: f32 = 4.0;
/// Nothing said this long after a call, what was heard as Clew was called
/// is worked out on its own: its words prove the call, or it was none —
/// and Clew, held for it, does not wait longer to go on.
pub(crate) const CALL_QUIET: f32 = 2.0;
/// Listening this long with nothing said, Clew quiet and nothing on its
/// way, it waits for its name again.
pub(crate) const IDLE: Duration = Duration::from_secs(180);
/// How often what the microphone heard is handed to the spotter: a piece
/// ([`clew_voice::wake::FEED_SECONDS`]) at a time.
pub(crate) const STEP: Duration = Duration::from_millis(200);

/// What hears the phrase: `clew-speak listen` ([`Spotters`]), or a test's.
pub(crate) trait Waking: Send + Sync {
    /// Load the model in `dir`.
    fn warm(&self, dir: &Path) -> Result<(), String>;
    /// Hand it `samples` ([`spot::RATE`]): whether the phrase was heard;
    /// `None` while it is not loaded.
    fn hear(&self, dir: &Path, samples: &[f32]) -> Result<Option<bool>, String>;
    /// Let go of what it heard so far.
    fn reset(&self);
    /// Let the model go.
    fn stop(&self);
}

impl Waking for Spotters {
    fn warm(&self, dir: &Path) -> Result<(), String> {
        Spotters::warm(self, dir)
    }
    fn hear(&self, dir: &Path, samples: &[f32]) -> Result<Option<bool>, String> {
        Spotters::hear(self, dir, samples)
    }
    fn reset(&self) {
        Spotters::reset(self);
    }
    fn stop(&self) {
        Spotters::stop(self);
    }
}

/// The one `clew-speak listen` of this process — if `clew-speak` is beside
/// Clew.
static SPOTTERS: LazyLock<Option<Arc<Spotters>>> =
    LazyLock::new(|| Some(Arc::new(Spotters::new(clew_core::embed::local::program()?))));

/// Whether a window is waiting: one at a time, as one spotter hears one
/// microphone — and "Hey, Clew" starts one window listening, not each.
static WAITER: AtomicBool = AtomicBool::new(false);
/// The last waiting's number.
static WAITS: AtomicU64 = AtomicU64::new(0);

/// One window waits at a time, the whole process over: the tests that have
/// one wait take turns.
#[cfg(test)]
pub(crate) static TEST_TURN: Mutex<()> = Mutex::new(());

/// A window's turn to wait, held while it does.
pub(crate) struct WaitClaim(());

impl WaitClaim {
    fn take() -> Option<WaitClaim> {
        WAITER
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| WaitClaim(()))
    }
}

impl Drop for WaitClaim {
    fn drop(&mut self) {
        WAITER.store(false, Ordering::Release);
    }
}

/// The microphone waiting, as it is heard off the window's thread.
#[derive(Default)]
pub(crate) struct Hearing {
    mic: Option<Box<dyn Listening>>,
    feed: Feed,
    /// The last [`HEARD_SECONDS`] heard, at the microphone's rate.
    recent: VecDeque<f32>,
    rate: u32,
    /// The phrase heard, or the waiting failed, and the window told:
    /// nothing more is taken from the microphone meanwhile — what is said
    /// next is listened to.
    told: bool,
    /// What the spotter heard before is let go of at the next step: a
    /// waiting begins afresh, and goes on afresh after a call it was not.
    fresh: bool,
    /// When the spotter failed lately: it is started again after each —
    /// one that fell behind while the computer was busy, or ended, hears
    /// again — until [`FAILURES`] within [`FAILING_WITHIN`] say it will not.
    /// Counted by time, not in a row: a spotter started again always hears
    /// its first pieces, so one that fails again and again never failed
    /// twice in a row.
    failures: VecDeque<Instant>,
}

/// The spotter's failures, within [`FAILING_WITHIN`] of each other, that end
/// the waiting: fewer are taken for a moment's trouble, and it is started
/// again.
pub(crate) const FAILURES: usize = 3;

/// How close together [`FAILURES`] are to end the waiting: a computer too
/// slow for the spotter makes it fall behind every few seconds.
pub(crate) const FAILING_WITHIN: Duration = Duration::from_secs(60);

/// What a step of waiting came to.
#[derive(Debug, PartialEq)]
pub(crate) enum Step {
    Nothing,
    Called,
    Failed(String),
}

/// One waiting: what its subscription is made of, and known by.
#[derive(Clone)]
pub(crate) struct Wait {
    pub(crate) id: u64,
    hearing: Arc<Mutex<Hearing>>,
    spotters: Arc<dyn Waking>,
    dir: PathBuf,
}

impl Hash for Wait {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl Wait {
    fn hearing(&self) -> MutexGuard<'_, Hearing> {
        self.hearing.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the waiting goes on: its microphone not handed back, the
    /// window not told it is over.
    fn waits(&self) -> bool {
        let hearing = self.hearing();
        hearing.mic.is_some() && !hearing.told
    }

    /// What the microphone heard since the last step, to the spotter: the
    /// model loaded first, if it is not.
    pub(crate) fn step(&self) -> Step {
        let (piece, fresh) = {
            let mut hearing = self.hearing();
            let hearing = &mut *hearing;
            if hearing.told {
                return Step::Nothing;
            }
            let Some(mic) = hearing.mic.as_mut() else {
                return Step::Nothing;
            };
            if let Some(e) = mic.error() {
                hearing.told = true;
                return Step::Failed(e);
            }
            let heard = mic.take();
            if heard.rate != hearing.rate {
                hearing.recent.clear();
                hearing.rate = heard.rate;
            }
            hearing.recent.extend(&heard.samples);
            let keep = (heard.rate as f32 * HEARD_SECONDS) as usize;
            let over = hearing.recent.len().saturating_sub(keep);
            hearing.recent.drain(..over);
            (
                hearing.feed.push(&heard.samples, heard.rate),
                std::mem::take(&mut hearing.fresh),
            )
        };
        if fresh {
            self.spotters.reset();
        }
        let Some(piece) = piece else {
            return Step::Nothing;
        };
        let heard = self.spotters.hear(&self.dir, &piece);
        let failing = heard.is_err() && {
            let mut hearing = self.hearing();
            let now = Instant::now();
            hearing
                .failures
                .retain(|at| now.duration_since(*at) < FAILING_WITHIN);
            hearing.failures.push_back(now);
            hearing.failures.len() >= FAILURES
        };
        let step = match heard {
            // Too soft for listening to hear — the spotter brings what it
            // hears up, listening does not: what was said next would not
            // be heard. As a call it was not: waiting goes on, afresh.
            Ok(Some(true)) if !self.heard_as_speech() => {
                self.hearing().fresh = true;
                Step::Nothing
            }
            Ok(Some(true)) => Step::Called,
            Ok(Some(false)) => Step::Nothing,
            // Not loaded — the first step, or the one after it failed: it
            // loads now, and hears from the next piece. Not for a waiting
            // over meanwhile, nor a model being removed (in any window).
            Ok(None) if !self.waits() => Step::Nothing,
            Ok(None)
                if crate::app::voice::downloading(&self.dir) || !spot::installed_at(&self.dir) =>
            {
                Step::Failed(tr!("the keyword spotter was removed").into())
            }
            Ok(None) => match self.spotters.warm(&self.dir) {
                // Over while it loaded: it is let go of again.
                Ok(()) if !self.waits() => {
                    self.spotters.stop();
                    Step::Nothing
                }
                Ok(()) => Step::Nothing,
                Err(e) => Step::Failed(e),
            },
            // The spotter is let go of as it fails, and started again at the
            // next step: a moment's trouble — the computer too busy for it
            // to keep up — does not end the waiting. Failing again and
            // again, it does.
            Err(e) if failing => Step::Failed(e),
            Err(_) => Step::Nothing,
        };
        if step != Step::Nothing {
            self.hearing().told = true;
        }
        step
    }

    /// Whether what was heard last ([`HEARD_SECONDS`], the phrase among it)
    /// holds speech as listening hears it.
    fn heard_as_speech(&self) -> bool {
        let (samples, rate) = {
            let hearing = self.hearing();
            (
                hearing.recent.iter().copied().collect::<Vec<f32>>(),
                hearing.rate,
            )
        };
        clew_voice::audio::holds_speech(&samples, rate)
    }

    /// The spotter's failures, as if each were `ago` earlier.
    #[cfg(test)]
    pub(crate) fn age_failures(&self, ago: Duration) {
        for at in self.hearing().failures.iter_mut() {
            *at = at.checked_sub(ago).unwrap_or(*at);
        }
    }
}

/// While a window waits: its microphone heard, a step every [`STEP`], off
/// the window's thread — the window hears only of the phrase, or of the
/// waiting failing.
pub(crate) fn subscription(wait: &Wait) -> Subscription<Message> {
    Subscription::run_with(wait.clone(), hear)
}

/// What the window is told of a step of waiting `id`: nothing, unless the
/// phrase was heard or the waiting failed.
pub(crate) fn told(id: u64, step: Step) -> Option<VoiceMsg> {
    match step {
        Step::Nothing => None,
        Step::Called => Some(VoiceMsg::Called(id)),
        Step::Failed(error) => Some(VoiceMsg::WaitFailed { wait: id, error }),
    }
}

fn hear(wait: &Wait) -> impl iced::futures::Stream<Item = Message> + use<> {
    let wait = wait.clone();
    iced::stream::channel(
        4,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            use iced::futures::SinkExt;
            let id = wait.id;
            loop {
                tokio::time::sleep(STEP).await;
                let stepping = wait.clone();
                let step = tokio::task::spawn_blocking(move || stepping.step())
                    .await
                    .unwrap_or_else(|_| Step::Failed(tr!("waiting failed unexpectedly").into()));
                let Some(message) = told(id, step) else {
                    continue;
                };
                if output.send(Message::Voice(message)).await.is_err() {
                    break;
                }
            }
        },
    )
}

/// The microphone, waiting.
pub(crate) struct Waiting {
    wait: Wait,
    _claim: WaitClaim,
}

impl Waiting {
    /// The microphone back, and what it heard last ([`HEARD_SECONDS`]).
    fn into_mic(self) -> Option<(Box<dyn Listening>, Recording)> {
        let mut hearing = self.wait.hearing();
        let mic = hearing.mic.take()?;
        let lead = Recording {
            samples: std::mem::take(&mut hearing.recent).into(),
            rate: hearing.rate,
        };
        Some((mic, lead))
    }
}

impl Drop for Waiting {
    /// The microphone closes now — not once the subscription hearing it
    /// has been let go.
    fn drop(&mut self) {
        let mic = self.wait.hearing().mic.take();
        if let Some(mic) = mic {
            drop(mic.stop());
        }
    }
}

/// Clew held for a call ([`WakeState::held`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct CallHold {
    /// The call, by its number.
    call: u64,
    /// Which hold it is
    /// ([`Speech::held_since`](crate::app::speech::Speech::held_since)):
    /// held again since, by words said over Clew, the call does not let it
    /// go on.
    since: Instant,
    /// What Clew was saying then
    /// ([`Speech::round`](crate::app::speech::Speech::round)): stopped
    /// since, it is not stopped again — gone on, held too long, it is.
    round: u64,
}

/// Waiting's state in a window.
pub(crate) struct WakeState {
    /// What hears the phrase; `None` when `clew-speak` is not beside Clew.
    pub(crate) spotters: Option<Arc<dyn Waking>>,
    pub(crate) download: Option<ModelDownload>,
    /// The last download's or removal's failure.
    pub(crate) error: Option<String>,
    /// The microphone, while it waits.
    waiting: Option<Waiting>,
    /// What was heard as the phrase was ([`HEARD_SECONDS`]), for what is
    /// worked out next: the call itself, whose name proves it, and what was
    /// said in one breath with it.
    pub(crate) lead: Option<Recording>,
    /// While listening: when something was last said — or Clew last spoke,
    /// or listening began.
    pub(crate) last_said: Instant,
    /// Listening began with "Hey, Clew", not the voice key: a while with
    /// nothing said has it wait for its name again.
    pub(crate) called: bool,
    /// The call listening began with, by its number, while Clew's name has
    /// not been heard in what was said since ([`App::prove_call`]).
    pub(crate) unproven: Option<u64>,
    /// The call last found to be none, which ended the listening it began
    /// ([`App::not_called`]) — nothing listened since: named after all in
    /// what was said after it, listening begins again.
    pub(crate) given_up: Option<u64>,
    /// Clew, held for a call as it spoke: it stops once the call is
    /// proven, and goes on if it was no call.
    pub(crate) held: Option<CallHold>,
    /// The last call's number.
    calls: u64,
    /// Why waiting failed: it is not tried again by itself — only once the
    /// reader turns it on again, or downloads the model: what failed (an
    /// old `clew-speak`, a model that does not load) fails again. Kept
    /// apart from [`WakeState::error`]: Settings' Try again, beside it,
    /// begins waiting again, not a download.
    pub(crate) failed: Option<String>,
}

impl WakeState {
    pub(crate) fn new() -> Self {
        Self {
            spotters: SPOTTERS.clone().map(|s| s as Arc<dyn Waking>),
            download: None,
            error: None,
            waiting: None,
            lead: None,
            last_said: Instant::now(),
            called: false,
            unproven: None,
            given_up: None,
            held: None,
            calls: 0,
            failed: None,
        }
    }

    /// Listening begins: for `call`, which its words are to prove — or
    /// for the reader, with none.
    pub(crate) fn listening_began(&mut self, call: Option<u64>) {
        self.called = call.is_some();
        self.unproven = call;
        self.given_up = None;
    }

    /// Listening ended — by the reader, or "Bye, Clew": no call it began
    /// waits to be proven, nor is one given up on. What was being heard is
    /// cut before ([`App::mic_off`]), and proves its call or not.
    pub(crate) fn listening_ended(&mut self) {
        self.unproven = None;
        self.given_up = None;
    }

    /// A number for a call just heard.
    fn next_call(&mut self) -> u64 {
        self.calls += 1;
        self.calls
    }

    /// Whether the microphone waits for the phrase.
    pub(crate) fn is_waiting(&self) -> bool {
        self.waiting.is_some()
    }

    /// The waiting, for its subscription.
    pub(crate) fn wait(&self) -> Option<&Wait> {
        self.waiting.as_ref().map(|w| &w.wait)
    }
}

impl App {
    /// The model's folder, when it is here, is not being downloaded or
    /// removed (in any window), and something can run it.
    pub(crate) fn wake_model(&self) -> Option<PathBuf> {
        let store = self.voice.store.as_ref()?;
        self.voice.wake.spotters.as_ref()?;
        let dir = spot::dir(store);
        (!crate::app::voice::downloading(&dir) && spot::installed(store)).then_some(dir)
    }

    /// Whether this window could wait for the phrase — the reader asked for
    /// it, it is not listening, waiting has not failed, and everything it
    /// needs is here — another window waiting aside.
    pub(crate) fn can_wait(&self) -> bool {
        self.voice.prefs.wake
            && !self.voice.mic_on
            && self.voice.wake.failed.is_none()
            // What is said once it is called needs a model to be heard with.
            && self.voice.installed(self.voice.prefs.model())
            && self.wake_model().is_some()
    }

    /// The microphone, not listening, waits for the phrase — if
    /// [`App::can_wait`], and no other window waits. Whether it waits.
    pub(crate) fn resume_waiting(&mut self) -> bool {
        self.wait_with(None)
    }

    /// [`App::resume_waiting`], with `mic` if one is given — the one that
    /// was listening, its echo canceller kept — else one opened for it. A
    /// `mic` not wanted is closed.
    pub(crate) fn wait_with(&mut self, mic: Option<Box<dyn Listening>>) -> bool {
        if self.voice.wake.waiting.is_some() {
            if let Some(mic) = mic {
                drop(mic.stop());
            }
            return true;
        }
        let start = self
            .can_wait()
            .then(|| self.wake_model())
            .flatten()
            .zip(self.voice.wake.spotters.clone())
            .and_then(|found| Some((found, WaitClaim::take()?)));
        let Some(((dir, spotters), claim)) = start else {
            if let Some(mic) = mic {
                drop(mic.stop());
            }
            return false;
        };
        let mic = match mic {
            Some(mut mic) => {
                drop(mic.take());
                Ok(mic)
            }
            None => (self.voice.open_mic)(MAX_SECONDS, self.voice.prefs.talk_over),
        };
        match mic {
            Ok(mic) => {
                let id = WAITS.fetch_add(1, Ordering::Relaxed) + 1;
                let hearing = Hearing {
                    mic: Some(mic),
                    fresh: true,
                    ..Hearing::default()
                };
                self.voice.wake.waiting = Some(Waiting {
                    wait: Wait {
                        id,
                        hearing: Arc::new(Mutex::new(hearing)),
                        spotters,
                        dir,
                    },
                    _claim: claim,
                });
                true
            }
            Err(e) => {
                self.wait_failed(e);
                false
            }
        }
    }

    /// Waiting failed, saying why: it is not tried again by itself — only
    /// once the reader turns it on again ([`WakeState::failed`]).
    fn wait_failed(&mut self, error: String) {
        self.status = tr!(
            "Stopped waiting for “Hey, Clew”: {error}",
            error = crate::i18n::message(&error)
        );
        self.voice.wake.failed = Some(error);
    }

    /// The microphone stops waiting.
    pub(crate) fn stop_waiting(&mut self) {
        self.voice.wake.waiting = None;
    }

    /// The microphone that waited, handed over — to wait in another window
    /// — and this one waiting no more.
    pub(crate) fn give_up_waiting(&mut self) -> Option<Box<dyn Listening>> {
        let (mic, _) = self.voice.wake.waiting.take()?.into_mic()?;
        Some(mic)
    }

    /// The microphone that waited, to listen with: what it heard just
    /// before kept, as the lead of what is worked out next.
    pub(crate) fn take_waiting_mic(&mut self) -> Option<Box<dyn Listening>> {
        let (mic, lead) = self.voice.wake.waiting.take()?.into_mic()?;
        self.voice.wake.lead = Some(lead);
        Some(mic)
    }

    /// The phrase, heard by waiting `wait`: listening, as the voice key
    /// starts it — Clew held, while it is heard whether it was a call.
    fn on_called(&mut self, wait: u64) -> Task<Message> {
        let Some(waiting) = &self.voice.wake.waiting else {
            return Task::none();
        };
        if waiting.wait.id != wait {
            return Task::none();
        }
        // Heard with Clew's voice in it, nothing taken out: Clew's own,
        // most likely — no call. Waiting goes on, afresh.
        if self.voice.speech.is_speaking() && !self.voice.prefs.talk_over {
            let mut hearing = waiting.wait.hearing();
            hearing.told = false;
            hearing.fresh = true;
            return Task::none();
        }
        let speaking = self.voice.speech.is_speaking() || self.answer_read_aloud_coming();
        // The model listened with, removed meanwhile: nothing to hear with,
        // nor to hear whether it was a call — said as the voice key says
        // it, and Clew stopped as the voice key stops it.
        if !self.voice.installed(self.voice.prefs.model()) {
            self.stop_waiting();
            if speaking {
                self.stop_speaking();
            }
            return self.listen_now(None);
        }
        let call = self.voice.wake.next_call();
        // Clew held, not stopped — an answer still on its way to being read
        // aloud held too, once it plays: if what is heard with the call
        // holds no "Clew", it was no call, and Clew goes on where it was.
        if speaking {
            self.voice.speech.hold();
            let round = self.voice.speech.round();
            self.voice.wake.held =
                self.voice
                    .speech
                    .held_since()
                    .map(|since| CallHold { call, since, round });
        }
        let task = self.listen_now(Some(call));
        // The microphone would not open: nothing will prove the call.
        if !self.voice.mic_on {
            self.call_unheard();
        }
        task
    }

    /// What the words heard after `call` make of it — Clew called by its
    /// name or not — once they are worked out (`result`): whether they are
    /// taken as said to Clew.
    ///
    /// Its name in them — whisper, told it, writes it: "Hey, Clew." 20
    /// times in 20 in recordings of the call, with Tiny in English — it was
    /// a call: Clew, held for it, stops, and what is heard since is said to
    /// Clew. Words without it — something like the call said to someone
    /// else ("Hey, cool, it works now"), which woke the spotter — it was
    /// not: Clew goes on, and none of it is done ([`App::not_called`]).
    /// Nothing written down, or nothing worked out, is no sign either way:
    /// Clew goes on, and what is said next decides.
    pub(crate) fn prove_call(&mut self, call: u64, result: &Result<String, String>) -> bool {
        let text = result.as_deref().map_or("", str::trim);
        if text.is_empty() {
            self.end_hold(call, false);
            return true;
        }
        // "Bye, Clew" is said to Clew too — and lets it go on reading: it
        // ends listening, not what Clew says. Heard while Clew spoke, its
        // own words heard back are no call.
        let held = self.voice.wake.held.is_some_and(|h| h.call == call);
        let stops = crate::app::spoken::says_bye(text, true);
        let goes_on = crate::app::spoken::leaves_clew_speaking(text, true);
        let to_clew = crate::app::spoken::named_in(text).is_some() || stops;
        let named = to_clew
            && (!held || crate::app::voice::said_over(text, &self.voice.speech.said_lately()));
        if !named {
            self.end_hold(call, false);
            self.not_called(call);
            return false;
        }
        if self.voice.wake.unproven == Some(call) {
            self.voice.wake.unproven = None;
        }
        self.voice.prove(call);
        self.end_hold(call, !goes_on);
        // Given up on — what was heard first was no call — and named after
        // all in what was said next, the spotter not listening meanwhile:
        // a call, and listening begins again for it, Clew stopped. Not for
        // a "Bye, Clew": it ends listening, which has ended.
        if self.voice.wake.given_up.take() == Some(call)
            && !stops
            && !self.voice.mic_on
            && self.voice.installed(self.voice.prefs.model())
        {
            if !goes_on && (self.voice.speech.is_speaking() || self.answer_read_aloud_coming()) {
                self.stop_speaking();
            }
            // With the model to hear with here, it opens no Settings.
            let _ = self.listen_now(None);
            self.voice.wake.called = true;
        }
        true
    }

    /// Clew, held for `call`: stopped — the call was one — or going on
    /// where it was. Stopped since — by the voice key, Esc, a question —
    /// it is left so; held again since, by words said over it, those words
    /// decide whether it goes on. Gone on by itself, held too long, it is
    /// still stopped for a call.
    fn end_hold(&mut self, call: u64, stop: bool) {
        let Some(hold) = self.voice.wake.held.filter(|h| h.call == call) else {
            return;
        };
        self.voice.wake.held = None;
        if stop {
            if self.voice.speech.round() == hold.round {
                self.stop_speaking();
            }
        } else if self.voice.speech.held_since() == Some(hold.since) {
            self.voice.speech.go_on();
        }
    }

    /// The spotter woke for what was not Clew's name, said after `call`
    /// ([`App::prove_call`]). Still listening for that call, listening ends,
    /// saying so, and the microphone waits for the name again; listening
    /// begun since — the voice key, another call — goes on. What was said
    /// after it, still to be worked out, is worked out and proves the call
    /// or not, each on its own: "Hey, Clew" said once someone else's
    /// "Hey, cool" woke Clew — the spotter not listening meanwhile — is a
    /// call still, and listening begins again for it
    /// ([`App::prove_call`]).
    pub(crate) fn not_called(&mut self, call: u64) {
        if self.voice.wake.unproven != Some(call) {
            return;
        }
        self.voice.wake.unproven = None;
        self.voice.heard = None;
        if self.voice.mic_on {
            self.voice.mic_on = false;
            self.wait_again();
            self.voice.wake.given_up = Some(call);
        }
        self.status = if self.resume_waiting() {
            tr!("Didn't hear “Clew” — waiting for “Hey, Clew”").into()
        } else if let Some(error) = &self.voice.wake.failed {
            tr!(
                "Stopped waiting for “Hey, Clew”: {error}",
                error = crate::i18n::message(error)
            )
        } else {
            tr!("Stopped listening").into()
        };
    }

    /// The voice key, pressed as a call is heard out: the listening is the
    /// reader's — what is said next is said to Clew, whatever the call
    /// turns out to have been. What is being said as it is pressed is cut
    /// there and checked as the call's, proving it or not; what was heard
    /// as it was called, nothing said since, is let go of.
    pub(crate) fn call_taken_over(&mut self) -> Task<Message> {
        if self.voice.wake.unproven.is_none() {
            return Task::none();
        }
        let hearing = self
            .voice
            .listening_now()
            .is_some_and(|l| l.silence_after_speech().is_some());
        let task = if hearing {
            self.end_utterance()
        } else {
            Task::none()
        };
        self.voice.wake.unproven = None;
        self.voice.wake.lead = None;
        // The reader's listening: it does not go back to waiting after a
        // while with nothing said, as listening begun by a call does.
        self.voice.wake.called = false;
        task
    }

    /// What the call Clew is listening for would be proven by — the moment
    /// heard with it — let go of unheard: Clew, held for it, goes on.
    pub(crate) fn call_unheard(&mut self) {
        if let Some(call) = self.voice.wake.unproven {
            self.end_hold(call, false);
        }
    }

    /// Waiting `wait` failed: it ends, saying why.
    fn on_wait_failed(&mut self, wait: u64, error: String) -> Task<Message> {
        if self.voice.wake.wait().is_none_or(|w| w.id != wait) {
            return Task::none();
        }
        self.stop_waiting();
        self.wait_failed(error);
        Task::none()
    }

    /// Turn waiting on or off.
    fn set_wake(&mut self, on: bool) {
        self.voice.prefs.wake = on;
        self.save_voice_pref(Pref::Wake);
        self.voice.wake.failed = None;
        if !on {
            self.voice.wake.given_up = None;
            self.stop_waiting();
            if let Some(spotters) = &self.voice.wake.spotters {
                spotters.stop();
            }
            return;
        }
        if self.resume_waiting() {
            self.status = tr!("Waiting for “Hey, Clew”").into();
            return;
        }
        // Why not, said — listening, it waits once it stops.
        if self.voice.mic_on {
            return;
        }
        let store = self.voice.store.as_ref();
        if self.voice.wake.spotters.is_none() {
            self.status = tr!("clew-speak, which runs it, is not installed beside Clew").into();
        } else if !store.is_some_and(spot::installed) && self.voice.wake.download.is_none() {
            self.status = tr!(
                "Download {model} ({size} MB) in Settings → Voice to start listening when you say “Hey, Clew”",
                model = crate::i18n::lookup(MODEL.name),
                size = MODEL.size_mb()
            );
        } else if !self.voice.installed(self.voice.prefs.model()) {
            let model = self.voice.prefs.model();
            self.status = tr!(
                "Voice needs a speech model on this computer — download {model} ({size} MB) in Settings → Voice",
                model = crate::i18n::lookup(model.name),
                size = model.size_mb
            );
        }
    }

    /// What the wake phrase's messages do.
    pub(crate) fn on_wake(&mut self, msg: VoiceMsg) -> Task<Message> {
        let name = crate::i18n::lookup(MODEL.name);
        match msg {
            VoiceMsg::SetWake(on) => {
                self.set_wake(on);
                Task::none()
            }
            VoiceMsg::Called(wait) => self.on_called(wait),
            VoiceMsg::WaitFailed { wait, error } => self.on_wait_failed(wait, error),
            VoiceMsg::DownloadWake => self.start_wake_download(),
            VoiceMsg::CancelWakeDownload => {
                if let Some(d) = self.voice.wake.download.take() {
                    d.cancel.store(true, Ordering::Relaxed);
                }
                Task::none()
            }
            VoiceMsg::WakeDownloadProgress { done, total } => {
                if let Some(d) = self.voice.wake.download.as_mut() {
                    d.done = done;
                    d.total = total;
                }
                Task::none()
            }
            VoiceMsg::WakeDownloaded(result) => {
                if self.voice.wake.download.take().is_none() {
                    return Task::none();
                }
                match result {
                    Ok(()) => {
                        self.voice.wake.error = None;
                        self.voice.wake.failed = None;
                        self.status = if self.resume_waiting() {
                            tr!("Waiting for “Hey, Clew”").into()
                        } else {
                            tr!("{model} is ready", model = name)
                        };
                    }
                    Err(e) => {
                        self.status = tr!(
                            "Could not download {model}: {error}",
                            model = name,
                            error = e
                        );
                        self.voice.wake.error = Some(e);
                    }
                }
                Task::none()
            }
            VoiceMsg::RemoveWake => self.remove_wake_model(),
            VoiceMsg::WakeRemoved(result) => {
                match result {
                    Ok(()) => {
                        // What failed with it is gone too.
                        self.voice.wake.error = None;
                        self.voice.wake.failed = None;
                        self.status = tr!("{model} removed", model = name);
                    }
                    Err(e) => {
                        self.status =
                            tr!("Could not remove {model}: {error}", model = name, error = e);
                        self.voice.wake.error = Some(e);
                    }
                }
                Task::none()
            }
            _ => Task::none(),
        }
    }

    /// Download the model (the button that says its size): the archive,
    /// checked against its size and checksum as it lands, then the files it
    /// runs from taken out of it, each checked.
    fn start_wake_download(&mut self) -> Task<Message> {
        if self.voice.wake.download.is_some() {
            return Task::none();
        }
        let name = crate::i18n::lookup(MODEL.name);
        let Some(store) = self.voice.store.clone() else {
            self.status = tr!("There is no data directory to keep the model in").into();
            return Task::none();
        };
        let claim = match Claim::take(spot::dir(&store), ClaimedFor::Download) {
            Ok(claim) => claim,
            Err(held) => {
                self.status = held.busy(name);
                return Task::none();
            }
        };
        self.voice.wake.error = None;
        let cancel = Arc::new(AtomicBool::new(false));
        let fetch = self.voice.fetch;
        let stream = iced::stream::channel(64, {
            let cancel = cancel.clone();
            move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                use iced::futures::SinkExt;
                let _stop = StopOnDrop(cancel.clone());
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
                let worker_cancel = cancel.clone();
                tokio::task::spawn_blocking(move || {
                    let result = (|| {
                        let mut install = spot::begin(&store).map_err(|e| {
                            tr!("could not save the model: {error}", error = e).to_string()
                        })?;
                        let mut last = Instant::now();
                        let mut progress = |done: u64, total: Option<u64>| {
                            if last.elapsed() >= Duration::from_millis(150) {
                                last = Instant::now();
                                let _ = tx.send(Message::Voice(VoiceMsg::WakeDownloadProgress {
                                    done,
                                    total,
                                }));
                            }
                        };
                        match fetch(
                            MODEL.url,
                            MODEL.bytes,
                            &worker_cancel,
                            &mut install,
                            &mut progress,
                        ) {
                            // Cancelled as it ended: not put in place — the
                            // model may be being removed by now.
                            Ok(_) if worker_cancel.load(Ordering::Relaxed) => {
                                Err(clew_core::llm::CANCELLED.to_string())
                            }
                            Ok(_) => {
                                install.finish().map(|_| ())?;
                                spot::unpack(&store, &worker_cancel).map_err(|e| {
                                    if e == spot::CANCELLED {
                                        clew_core::llm::CANCELLED.to_string()
                                    } else {
                                        e
                                    }
                                })
                            }
                            Err(e) => Err(crate::app::voice::download_failure_sized(
                                &e,
                                install.written(),
                                MODEL.size_mb(),
                            )),
                        }
                    })();
                    let _ = tx.send(Message::Voice(VoiceMsg::WakeDownloaded(result)));
                });
                while let Some(message) = rx.recv().await {
                    let done = matches!(message, Message::Voice(VoiceMsg::WakeDownloaded(..)));
                    if output.send(message).await.is_err() || done {
                        break;
                    }
                }
            }
        });
        let (task, handle) = Task::run(stream, |m| m).abortable();
        self.voice.wake.download = Some(ModelDownload {
            done: 0,
            total: Some(MODEL.bytes),
            cancel,
            _claim: claim,
            _task: handle.abort_on_drop(),
        });
        self.status = tr!(
            "Downloading {model} ({size} MB)…",
            model = name,
            size = MODEL.size_mb()
        );
        task
    }

    /// Remove the model: the microphone stops waiting, and what runs the
    /// model lets go of it first — Windows will not delete a file in use.
    fn remove_wake_model(&mut self) -> Task<Message> {
        let Some(store) = self.voice.store.clone() else {
            return Task::none();
        };
        let claim = match Claim::take(spot::dir(&store), ClaimedFor::Removal) {
            Ok(claim) => claim,
            Err(held) => {
                self.status = held.busy(crate::i18n::lookup(MODEL.name));
                return Task::none();
            }
        };
        self.stop_waiting();
        let spotters = self.voice.wake.spotters.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let _claim = claim;
                    if let Some(spotters) = spotters {
                        spotters.stop();
                    }
                    spot::remove(&store).map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|_| Err(tr!("the removal failed unexpectedly").into()))
            },
            |result| Message::Voice(VoiceMsg::WakeRemoved(result)),
        )
    }
}

/// A waiting that hears nothing, for tests of what is around it.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use clew_voice::models::Store;

    /// A microphone that hears nothing.
    pub(crate) struct Unheard;

    impl Listening for Unheard {
        fn level(&self) -> f32 {
            0.0
        }
        fn silence_after_speech(&self) -> Option<f32> {
            None
        }
        fn speech_over(&self) -> Option<f32> {
            None
        }
        fn recent(&self, _seconds: f32) -> (Vec<f32>, u32) {
            (Vec::new(), spot::RATE)
        }
        fn full(&self) -> bool {
            false
        }
        fn error(&self) -> Option<String> {
            None
        }
        fn take(&mut self) -> Recording {
            Recording {
                samples: Vec::new(),
                rate: spot::RATE,
            }
        }
        fn stop(self: Box<Self>) -> Recording {
            Recording {
                samples: Vec::new(),
                rate: spot::RATE,
            }
        }
    }

    /// How many microphones [`ready`] opened.
    pub(crate) static OPENED: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// A keyword spotter that never hears the phrase.
    pub(crate) struct NeverCalled;

    impl Waking for NeverCalled {
        fn warm(&self, _dir: &Path) -> Result<(), String> {
            Ok(())
        }
        fn hear(&self, _dir: &Path, _samples: &[f32]) -> Result<Option<bool>, String> {
            Ok(Some(false))
        }
        fn reset(&self) {}
        fn stop(&self) {}
    }

    /// `app` able to wait, in `store`: the model chosen and the keyword
    /// spotter "downloaded" there, a microphone that hears nothing and a
    /// spotter that never hears the phrase — and waiting turned on.
    pub(crate) fn ready(app: &mut App, store: Store) {
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(store.path(app.voice.prefs.model()), b"a model").unwrap();
        let dir = spot::dir(&store);
        std::fs::create_dir_all(&dir).unwrap();
        for file in MODEL.files {
            std::fs::File::create(dir.join(file.name))
                .unwrap()
                .set_len(file.bytes)
                .unwrap();
        }
        app.voice.store = Some(store);
        app.voice.wake.spotters = Some(Arc::new(NeverCalled));
        app.voice.open_mic = |_, _| {
            OPENED.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(Unheard))
        };
        app.voice.prefs.wake = true;
    }
}
