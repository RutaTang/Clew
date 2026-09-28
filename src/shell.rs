//! The multi-window shell.
//!
//! clew runs under an iced daemon: this layer holds one independent [`App`] per
//! window and routes each window's messages back to *its own* `App`. The trick
//! is `map`: a window's view maps its `Message`s to `Shell::Window(id, _)`, so a
//! click and the async Tasks it spawns always return to the window that produced
//! them. Global input events carry the originating window id from `listen_with`;
//! the app-global menu bar targets the focused window via `Shell::ToFocused`,
//! except for the two commands that are the shell's own: New Window and Quit.
//!
//! A window goes only through [`Clew::close_window`], which runs its teardown,
//! and clew exits only once no window is left and no teardown is still running
//! ([`Clew::exit_if_done`]). The work a window owns is not all held by its
//! `App` — a debug adapter and its debuggee belong to a stream on the daemon's
//! runtime, and the only thing that stops them is a teardown that runs
//! *before* the process goes away. That includes the quit an update install
//! asks for: it is [`Shell::Quit`], every window's teardown, not just the
//! installing window's. It includes the quit macOS asks for too — the Dock's
//! Quit item, a log-out — which AppKit hands over as [`Shell::Terminate`] (see
//! `crate::macos::terminate`). What that CANNOT cover is a process death clew
//! is not asked about: Force Quit, a signal, or a crash, and there the adapter
//! is still on its own.
//!
//! Nothing is torn down before it is asked about. A window asked to close
//! sends the bookmark, note and walkthrough edits its journal holds, and
//! closes once its host has confirmed all of them — sent is not saved over a
//! link that died without anyone noticing. What it would leave unsaved once
//! the host has answered, or the grace is over — edits no transport took,
//! edits the host has not confirmed, edits the host could not save — is
//! asked about in one question, in a sheet on the window
//! ([`Shell::CloseRequested`]); kept open, the window goes on sending them. A
//! quit sends and waits the same way for every window, and asks once, for
//! all of them, before any window's teardown. Once the user has agreed,
//! teardown and exit follow at once: no exit waits on a question, and no
//! window stays usable after its teardown.
//!
//! A question is asked where the user sees it — its window brought forward,
//! and clew with it when the user asked for the close in clew or macOS waits
//! on the answer ([`Asker`]) — and its sheet ends the sheets on the window
//! first, so it never waits behind one. A question that no longer stands —
//! its window closing, a quit taking its place, its edits saved after all —
//! is withdrawn, and a sheet not begun by then never is. No window is closed
//! with a sheet on it: its sheets are ended first — clew's questions, and
//! the file pickers, which are sheets on the window they were opened from.
//!
//! Key presses a focused widget already consumed (a character typed into a text
//! field, ⌥← moving its caret) are NOT forwarded to the window's key handler —
//! see [`forward_key`] — so typing never drives the reading motions behind it.
//!
//! The keymap is global (one `[keymap]` in config.toml) while every window holds
//! its own copy. When one window's copy changes, the shell hands it to the other
//! windows and rebuilds the menu bar from it, so every window and the menu fire
//! on the same chords.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use iced::futures::FutureExt;
use iced::futures::future::BoxFuture;
use iced::{Element, Subscription, Task, Theme, keyboard, window};

use crate::app::UnsavedEdits;
use crate::{App, Message};
use crate::{EditorMsg, SettingsMsg, UpdaterMsg, WindowMsg};

/// The daemon's top-level state: one `App` per open window.
pub struct Clew {
    windows: HashMap<window::Id, App>,
    /// The window that most recently gained focus — the target of the app-global
    /// menu bar and any "current window" action.
    focused: Option<window::Id>,
    /// The windows asked to close and not closed yet: where each close
    /// stands — its edits sent and its host's answers awaited, or its
    /// question on screen — and who asked for it.
    closing: HashMap<window::Id, Close>,
    /// A quit under way, and where it stands: every window's edits sent and
    /// their hosts' answers awaited, or its question on screen.
    quitting: Option<Pending>,
    /// The windows asked to close while a quit went on for every window —
    /// waiting on the hosts, or asking on them — and who asked: they close
    /// with the rest when it goes ahead, and as asked when it is cancelled.
    held_closes: HashMap<window::Id, Asker>,
    /// macOS asked to terminate ([`Shell::Terminate`]) and waits for the
    /// answer.
    terminating: bool,
    /// The teardowns still running, by ticket ([`Clew::track`]): the exit
    /// waits for them.
    tearing_down: HashSet<u64>,
    /// The exit has been issued (or left to AppKit).
    exiting: bool,
    /// The last ticket handed out, to a question, a wait or a teardown.
    tickets: u64,
    /// What the shell leaves to the platform.
    platform: Box<dyn Platform>,
}

/// Who asked for a close, or a quit — which says how far its question may
/// go to be seen ([`Platform::present`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asker {
    /// The user, in clew — its close control, ⌘W, ⌘Q, an update's install:
    /// clew comes forward for the question, even from behind an app the user
    /// went to while the close waited on the host.
    User,
    /// Something outside clew closed the window — a window manager, a
    /// script: clew comes forward only when it is in front already, and
    /// otherwise its Dock icon only asks for attention, once.
    Outside,
    /// macOS — the Dock's Quit, a log-out, a restart — which waits on the
    /// answer: clew comes forward, and its Dock icon asks for attention
    /// until it has.
    System,
}

/// A close under way.
struct Close {
    /// Where it stands.
    pending: Pending,
    /// Who asked for it.
    by: Asker,
}

/// Where a close, or a quit, stands before it goes ahead.
enum Pending {
    /// The journals were sent ([`App::flush_remote_edits`]), and it waits
    /// for the hosts' answers to all they sent but `agreed`, the edits the
    /// user agreed to lose — until the grace `ticket` names is over
    /// ([`Shell::Waited`]).
    Confirming {
        ticket: u64,
        agreed: HashSet<String>,
    },
    /// Its question is on screen, in a sheet on window `on`.
    Asking { on: window::Id, asked: Asked },
}

impl Pending {
    /// Whether this is question `ticket`, on screen.
    fn asks(&self, ticket: u64) -> bool {
        matches!(self, Pending::Asking { asked, .. } if asked.ticket == ticket)
    }

    /// Whether this waits on the hosts until the grace `ticket` names is
    /// over.
    fn waits(&self, ticket: u64) -> bool {
        matches!(self, Pending::Confirming { ticket: t, .. } if *t == ticket)
    }
}

/// A question on screen.
struct Asked {
    /// What its answer comes back under ([`Shell::Answered`]).
    ticket: u64,
    /// The edits the user agreed to lose before it was asked, by id.
    agreed: HashSet<String>,
    /// The edits it names, by id.
    named: HashSet<String>,
    /// Set once it no longer stands — its window closing, a quit taking its
    /// place, its edits saved after all — and its answer is not wanted: its
    /// window is not brought forward for it, and its sheet, not begun by
    /// then, never is ([`Platform::ask`]).
    withdrawn: Arc<AtomicBool>,
}

impl Asked {
    fn new(ticket: u64, lost: &[UnsavedEdits], agreed: HashSet<String>) -> Asked {
        Asked {
            ticket,
            agreed,
            named: lost.iter().flat_map(|l| l.ids.iter().cloned()).collect(),
            withdrawn: Arc::default(),
        }
    }

    /// Every edit the user agrees to lose by answering it so: the ones it
    /// names, and the ones agreed to before it was asked.
    fn lost(&self) -> HashSet<String> {
        self.agreed.union(&self.named).cloned().collect()
    }

    /// It no longer stands.
    fn withdraw(&self) {
        self.withdrawn.store(true, Ordering::SeqCst);
    }
}

/// Whether `agreed` holds every edit `lost` names: none of them is lost
/// without the user having agreed.
fn covered(agreed: &HashSet<String>, lost: &[UnsavedEdits]) -> bool {
    lost.iter()
        .flat_map(|l| &l.ids)
        .all(|id| agreed.contains(id))
}

/// Whether `app` holds nothing it would leave unsaved beyond `agreed`.
fn saved(app: &App, agreed: &HashSet<String>) -> bool {
    app.unsaved_edits()
        .is_none_or(|lost| covered(agreed, std::slice::from_ref(&lost)))
}

/// A question the shell asks before edits are lost that cannot be sent, or
/// that their host has not confirmed or could not save: shown in a sheet on
/// a window, with two buttons.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Question {
    title: String,
    message: String,
    /// The default button, which keeps the edits: the window, or clew, stays
    /// open.
    keep: &'static str,
    /// The other, which gives them up.
    lose: &'static str,
}

/// The buttons of a window's question.
const KEEP_WINDOW: &str = "Keep Window Open";
const CLOSE_ANYWAY: &str = "Close Anyway";
/// The buttons of a quit's question.
const CANCEL_QUIT: &str = "Cancel";
const QUIT_ANYWAY: &str = "Quit Anyway";

/// What the shell leaves to the platform: showing its questions, waiting on
/// the clew-servers, and answering macOS. Injected, so a test's `update`
/// never shows a sheet, waits on another test's servers, or talks to AppKit.
trait Platform {
    /// Bring `window` where the user sees it before a question is asked on
    /// it — as far as `asker` allows — unless the question is `withdrawn`
    /// by then.
    fn present(&self, window: window::Id, asker: Asker, withdrawn: Arc<AtomicBool>) -> Task<()>;
    /// Ask `question` in a sheet on `window`, ending the sheets on it first
    /// — a file picker's, a question it takes the place of — so it is never
    /// queued behind one: `true` once the user chose to lose the edits,
    /// `false` for anything else — the default button, the sheet ended, a
    /// window gone before its sheet could show, or a question `withdrawn` by
    /// then, whose sheet is never begun.
    fn ask(&self, window: window::Id, question: Question, withdrawn: Arc<AtomicBool>)
    -> Task<bool>;
    /// End the sheets on `window`, each answered as if cancelled: before it
    /// closes, and when the question they show no longer stands.
    fn dismiss(&self, window: window::Id) -> Task<()>;
    /// Resolves once every clew-server this process spawned has been reaped.
    fn servers_reaped(&self) -> BoxFuture<'static, ()>;
    /// How long an exit waits for one teardown at most — and a close, or a
    /// quit, for the hosts to confirm the edits it sent.
    fn grace(&self) -> Duration;
    /// Answer macOS's request to terminate ([`Shell::Terminate`]): `true`
    /// lets it terminate the process, `false` keeps clew running.
    fn reply_terminate(&self, terminate: bool);
}

/// The platform clew runs on: questions in native sheets (rfd, an `NSAlert`
/// on macOS), and AppKit's terminate request answered to AppKit.
struct Native;

impl Platform for Native {
    fn present(&self, window: window::Id, asker: Asker, withdrawn: Arc<AtomicBool>) -> Task<()> {
        bring_forward(window, asker, withdrawn)
    }

    fn ask(
        &self,
        window: window::Id,
        question: Question,
        withdrawn: Arc<AtomicBool>,
    ) -> Task<bool> {
        sheet(window, question, withdrawn)
    }

    fn dismiss(&self, window: window::Id) -> Task<()> {
        end_sheets(window)
    }

    fn servers_reaped(&self) -> BoxFuture<'static, ()> {
        crate::server::all_servers_reaped().boxed()
    }

    fn grace(&self) -> Duration {
        TEARDOWN_GRACE
    }

    fn reply_terminate(&self, terminate: bool) {
        #[cfg(target_os = "macos")]
        crate::macos::terminate::reply(terminate);
        #[cfg(not(target_os = "macos"))]
        let _ = terminate;
    }
}

/// Bring `window` where the user sees it, from its own callback, on the main
/// thread (see `crate::macos::window::bring_forward`) — unless the question
/// is `withdrawn` by then.
#[cfg(target_os = "macos")]
fn bring_forward(window: window::Id, asker: Asker, withdrawn: Arc<AtomicBool>) -> Task<()> {
    // Whether clew comes forward from behind the app the user is in, and
    // whether its Dock icon asks for attention until it has, or once.
    let (activate, critical) = match asker {
        Asker::User => (true, false),
        Asker::Outside => (false, false),
        Asker::System => (true, true),
    };
    iced::window::run(window, move |window| {
        if !withdrawn.load(Ordering::SeqCst) {
            crate::macos::window::bring_forward(window, activate, critical);
        }
    })
}

/// Bring `window` where the user sees it, as far as iced can say so.
#[cfg(not(target_os = "macos"))]
fn bring_forward(window: window::Id, asker: Asker, withdrawn: Arc<AtomicBool>) -> Task<()> {
    // iced's window operations cannot look at the question as they run.
    let _ = withdrawn;
    let restore = iced::window::minimize(window, false);
    if asker == Asker::Outside {
        return restore;
    }
    Task::batch([restore, iced::window::gain_focus(window)])
}

/// End the sheets on `window`, from its own callback, on the main thread
/// (see `crate::macos::window::end_sheets`).
#[cfg(target_os = "macos")]
fn end_sheets(window: window::Id) -> Task<()> {
    iced::window::run(window, |window| crate::macos::window::end_sheets(window))
}

/// End the sheets on `window`: rfd's dialogs elsewhere cannot be ended from
/// here.
#[cfg(not(target_os = "macos"))]
fn end_sheets(window: window::Id) -> Task<()> {
    let _ = window;
    Task::none()
}

/// `question` in a sheet on `window`, answering whether the user chose to
/// lose the edits.
///
/// Window-modal, so the question stays with the window it is about, and it
/// is shown from the window's own callback, on the main thread, which is
/// where AppKit wants its sheets begun — after the sheets on the window are
/// ended, in the same callback, so nothing is begun between. A window that
/// closed before the callback ran gets none: the callback is dropped and
/// its channel with it, which answers "keep". So does a question withdrawn
/// by then: a quit took its place, or its window is closing, and its sheet
/// would have been queued behind the one that replaced it.
fn sheet(window: window::Id, question: Question, withdrawn: Arc<AtomicBool>) -> Task<bool> {
    let Question {
        title,
        message,
        keep,
        lose,
    } = question;
    iced::window::run(window, move |parent| {
        if withdrawn.load(Ordering::SeqCst) {
            return None;
        }
        #[cfg(target_os = "macos")]
        crate::macos::window::end_sheets(parent);
        Some(
            rfd::AsyncMessageDialog::new()
                .set_level(rfd::MessageLevel::Warning)
                .set_title(title)
                .set_description(message)
                // The first button is the default (Return), so `keep` is.
                .set_buttons(rfd::MessageButtons::OkCancelCustom(
                    keep.into(),
                    lose.into(),
                ))
                .set_parent(parent)
                .show(),
        )
    })
    .collect()
    .then(move |shown| match shown.into_iter().flatten().next() {
        Some(answer) => Task::future(answer)
            .map(move |answer| answer == rfd::MessageDialogResult::Custom(lose.into())),
        None => Task::done(false),
    })
}

/// The daemon's message: window-scoped app messages plus window lifecycle.
#[derive(Debug, Clone)]
pub enum Shell {
    /// Deliver an app message to a specific window's `App`.
    Window(window::Id, Message),
    /// Deliver an app message to the focused window (from the app-global menu).
    ToFocused(Message),
    /// Open a new, independent window.
    NewWindow,
    /// A window finished opening — set up its native chrome (main thread).
    Opened(window::Id),
    /// The OS asked a window to close — a window manager, a script. It
    /// goes once its host has confirmed the edits it sends as it goes, and
    /// what it would leave unsaved is asked about first — as when clew's
    /// own close control or ⌘W asks (`WindowMsg::Close`).
    CloseRequested(window::Id),
    /// A window closed. Normally one [`Clew::close_window`] tore down as it
    /// closed it; one that closed any other way is torn down now.
    Closed(window::Id),
    /// Quit the app: every window sends its edits and waits for its host,
    /// what they would leave unsaved is asked about, and then every window
    /// is torn down and clew exits.
    Quit,
    /// macOS asked the app to terminate — the Dock's Quit item, a log-out, a
    /// restart — and waits for the answer: a [`Shell::Quit`] whose outcome
    /// AppKit is told.
    Terminate,
    /// The user answered question `ticket`: `lose` when they chose to lose
    /// the edits it was about.
    Answered { ticket: u64, lose: bool },
    /// The close or quit waiting under `ticket` has waited its grace for the
    /// hosts to confirm what it sent.
    Waited(u64),
    /// The teardown `ticket` finished, or its grace ran out.
    TornDown(u64),
}

/// How long a quit waits for the windows' teardown before exiting regardless
/// — and a close, or a quit, for the hosts to confirm the edits it sent.
///
/// The teardown's own bound is the DAP client's 60 s request timeout, which is a
/// reasonable wait for a live adapter and a terrible one for ⌘Q: an adapter that
/// has stopped answering would hold the whole app open. Long enough for a
/// `disconnect` round trip on a working adapter, short enough that a wedged one
/// is not felt. The same holds for a host's answer to an edit: a working link
/// answers in far less, and a dead one is asked about rather than waited on.
const TEARDOWN_GRACE: Duration = Duration::from_secs(2);

/// Open the first window and seed its `App`.
pub fn boot() -> (Clew, Task<Shell>) {
    let mut clew = Clew::new(Box::new(Native));
    // Downloads abandoned by a clew that died mid-stream are nobody's to clean
    // up at runtime, and the data root they now live in is never swept by the
    // OS. Off-thread: it can be removing a disk image, and nothing waits on it.
    std::thread::spawn(crate::updater::sweep_stale_downloads);
    // The first window restores the last-opened project; New Window opens empty.
    let task = clew.open_window(true);
    (clew, task)
}

impl Clew {
    /// No window yet, on `platform`.
    fn new(platform: Box<dyn Platform>) -> Clew {
        Clew {
            windows: HashMap::new(),
            focused: None,
            closing: HashMap::new(),
            quitting: None,
            held_closes: HashMap::new(),
            terminating: false,
            tearing_down: HashSet::new(),
            exiting: false,
            tickets: 0,
            platform,
        }
    }

    /// A ticket not handed out before.
    fn ticket(&mut self) -> u64 {
        self.tickets += 1;
        self.tickets
    }

    /// Open a new window with a fresh `App`. `restore` reopens the last project
    /// (used for the first window at launch); otherwise the window starts empty,
    /// ready for the user to open a folder — the standard "New Window".
    fn open_window(&mut self, restore: bool) -> Task<Shell> {
        let (mut app, init) = if restore {
            App::new()
        } else {
            (App::blank(), Task::none())
        };
        let (id, open) = iced::window::open(crate::window_settings());
        // The App targets window operations (close / minimize / fullscreen) at
        // its own window.
        app.main_window = Some(id);
        self.windows.insert(id, app);
        self.focused = Some(id);
        Task::batch([
            init.map(move |m| Shell::Window(id, m)),
            // The window actually opens when this Task runs; its id output is
            // unused (chrome setup happens on the Window::Opened event instead).
            open.map(move |_| Shell::Window(id, Message::Noop)),
        ])
    }

    /// Window `id` was asked to close, by `by`: it goes once its host has
    /// confirmed what it sends as it goes, and what it would leave unsaved
    /// is asked about first ([`Clew::send_then_close`]).
    fn request_close(&mut self, id: window::Id, by: Asker) -> Task<Shell> {
        if !self.windows.contains_key(&id) {
            return Task::none();
        }
        // A quit that waits on every window's host, or asks on this one,
        // closes it with the rest. The close is kept, not dropped: if the
        // quit is cancelled, the window closes as asked here.
        let quit_holds = match &self.quitting {
            Some(Pending::Confirming { .. }) => true,
            Some(Pending::Asking { on, .. }) => *on == id,
            None => false,
        };
        if quit_holds {
            self.held_closes.entry(id).or_insert(by);
            return Task::none();
        }
        // Its close is under way already — waiting on its host, which its
        // status line says, or asking: it goes on there.
        if self.closing.contains_key(&id) {
            return Task::none();
        }
        self.send_then_close(id, by, HashSet::new())
    }

    /// Window `id` may close, losing `agreed` — the edits the user agreed
    /// to lose; none before anything was asked. It sends what its journal
    /// holds, and waits for its host's answers to all of it but `agreed`, up
    /// to the grace ([`Clew::waited`]): sent is not saved, as a transport
    /// that died without anyone noticing takes frames into a pipe that goes
    /// nowhere, and SSH notices only after about 45 s. The window stays open
    /// and working meanwhile, and says why on its status line; nothing
    /// blocks. Then it goes on ([`Clew::decide_close`]).
    fn send_then_close(
        &mut self,
        id: window::Id,
        by: Asker,
        agreed: HashSet<String>,
    ) -> Task<Shell> {
        let Some(app) = self.windows.get_mut(&id) else {
            return Task::none();
        };
        app.flush_remote_edits();
        if !app.awaits_host(&agreed) {
            return self.decide_close(id, by, agreed);
        }
        app.show_waiting(false);
        let ticket = self.ticket();
        let pending = Pending::Confirming { ticket, agreed };
        self.closing.insert(id, Close { pending, by });
        self.wait_grace(ticket)
    }

    /// Window `id`'s host has answered what its close sent, or the grace is
    /// over: the window closes — unless it would leave edits unsaved beyond
    /// `agreed`. Those are asked about, all of them in one question, whatever
    /// the reason: no transport took them, the host has not confirmed them,
    /// or the host could not save them.
    fn decide_close(&mut self, id: window::Id, by: Asker, agreed: HashSet<String>) -> Task<Shell> {
        let Some(app) = self.windows.get_mut(&id) else {
            return Task::none();
        };
        app.end_waiting();
        match app
            .unsaved_edits()
            .filter(|lost| !covered(&agreed, std::slice::from_ref(lost)))
        {
            Some(lost) => self.ask_close(id, by, lost, agreed),
            None => self.close_window(id),
        }
    }

    /// Ask, in a sheet on window `id`, whether it may close and lose `lost`
    /// — and `agreed`, the edits the user agreed to lose already.
    fn ask_close(
        &mut self,
        id: window::Id,
        by: Asker,
        lost: UnsavedEdits,
        agreed: HashSet<String>,
    ) -> Task<Shell> {
        let (title, message) = crate::app::close_question(&lost);
        let question = Question {
            title,
            message,
            keep: KEEP_WINDOW,
            lose: CLOSE_ANYWAY,
        };
        let asked = Asked::new(self.ticket(), std::slice::from_ref(&lost), agreed);
        let task = self.ask_on(id, question, &asked, by);
        let pending = Pending::Asking { on: id, asked };
        self.closing.insert(id, Close { pending, by });
        task
    }

    /// Ask `question` — `asked` — in a sheet on window `on`, brought where
    /// the user sees it first, as far as `by` allows. rfd only begins the
    /// sheet, and one begun on a hidden app or a minimized window was a
    /// question nobody saw: the quit waiting on it seemed to hang, and a
    /// log-out ended with macOS saying clew interrupted it.
    fn ask_on(&self, on: window::Id, question: Question, asked: &Asked, by: Asker) -> Task<Shell> {
        let ticket = asked.ticket;
        let present = self
            .platform
            .present(on, by, asked.withdrawn.clone())
            .discard();
        let ask = self
            .platform
            .ask(on, question, asked.withdrawn.clone())
            .map(move |lose| Shell::Answered { ticket, lose });
        present.chain(ask)
    }

    /// [`Shell::Waited`] under `ticket`, once the grace is over.
    fn wait_grace(&self, ticket: u64) -> Task<Shell> {
        let grace = self.platform.grace();
        // Built inside the future so no timer is armed until this runs.
        Task::future(async move { tokio::time::sleep(grace).await })
            .discard()
            .chain(Task::done(Shell::Waited(ticket)))
    }

    /// Window `id` has taken a message — maybe its host's answer, maybe an
    /// edit made while it waits.
    ///
    /// A close, or a quit, that waits on the hosts sends such an edit at
    /// once, as it sent the rest, and goes on once nothing it waits for is
    /// left unanswered. A question whose edits are all saved since it was
    /// asked no longer stands: it is withdrawn, its sheet ended, and the
    /// close, or the quit, goes on as if it had been answered.
    fn settle(&mut self, id: window::Id) -> Task<Shell> {
        let Some(app) = self.windows.get_mut(&id) else {
            return Task::none();
        };
        if let Some(close) = self.closing.get(&id) {
            let over = match &close.pending {
                Pending::Confirming { agreed, .. } => {
                    app.send_closing_edits();
                    let waits = app.awaits_host(agreed);
                    if waits {
                        app.update_waiting(false);
                    }
                    !waits
                }
                Pending::Asking { asked, .. } => saved(app, &asked.agreed),
            };
            if !over {
                return Task::none();
            }
            return match self.closing.remove(&id) {
                Some(close) => self.go_on_closing(id, close),
                None => Task::none(),
            };
        }
        let over = match &self.quitting {
            Some(Pending::Confirming { agreed, .. }) => {
                app.send_closing_edits();
                if app.awaits_host(agreed) {
                    app.update_waiting(true);
                } else {
                    app.end_waiting();
                }
                !self.windows.values().any(|app| app.awaits_host(agreed))
            }
            Some(Pending::Asking { asked, .. }) => {
                self.windows.values().all(|app| saved(app, &asked.agreed))
            }
            None => false,
        };
        if !over {
            return Task::none();
        }
        match self.quitting.take() {
            Some(Pending::Confirming { agreed, .. }) => self.decide_quit(agreed),
            Some(Pending::Asking { on, asked }) => {
                asked.withdraw();
                let dismissed = self.platform.dismiss(on).discard();
                dismissed.chain(self.send_then_quit(asked.agreed))
            }
            None => Task::none(),
        }
    }

    /// Window `id`'s close no longer waits: its host has answered all it
    /// sent, or the grace is over — or its question no longer stands, what
    /// it named saved since. It goes on.
    fn go_on_closing(&mut self, id: window::Id, close: Close) -> Task<Shell> {
        match close.pending {
            Pending::Confirming { agreed, .. } => self.decide_close(id, close.by, agreed),
            Pending::Asking { on, asked } => {
                asked.withdraw();
                let dismissed = self.platform.dismiss(on).discard();
                dismissed.chain(self.send_then_close(id, close.by, asked.agreed))
            }
        }
    }

    /// The close or quit waiting under `ticket` has waited its grace for
    /// the hosts: it goes on, and asks about what they have not confirmed.
    fn waited(&mut self, ticket: u64) -> Task<Shell> {
        let window = self
            .closing
            .iter()
            .find(|(_, close)| close.pending.waits(ticket))
            .map(|(id, _)| *id);
        if let Some(id) = window
            && let Some(close) = self.closing.remove(&id)
        {
            return self.go_on_closing(id, close);
        }
        if self.quitting.as_ref().is_some_and(|q| q.waits(ticket))
            && let Some(Pending::Confirming { agreed, .. }) = self.quitting.take()
        {
            return self.decide_quit(agreed);
        }
        // It went ahead already, or a quit took it over.
        Task::none()
    }

    /// Quit: once every window has sent what its journal holds and the
    /// hosts have answered, or the grace is over — and once the user has
    /// agreed to lose what they would leave unsaved, asked once, for every
    /// window, before anything is torn down ([`Clew::send_then_quit`]).
    fn quit(&mut self) -> Task<Shell> {
        // It is under way already: its question is up, and answered there,
        // or it waits on the hosts.
        if self.quitting.is_some() {
            return Task::none();
        }
        self.send_then_quit(HashSet::new())
    }

    /// What `pick` says of each window it says something of: one entry per
    /// window, in the order of their projects.
    fn everywhere(&self, pick: impl Fn(&App) -> Option<UnsavedEdits>) -> Vec<UnsavedEdits> {
        let mut lost: Vec<UnsavedEdits> = self.windows.values().filter_map(pick).collect();
        lost.sort_by(|a, b| a.project.cmp(&b.project));
        lost
    }

    /// Quit, losing `agreed` — the edits the user agreed to lose; none when
    /// nothing was asked. It takes over every close under way, and what
    /// their users agreed to lose. Every window sends what its journal
    /// holds, and clew waits for the hosts' answers to all of it but
    /// `agreed`, up to the grace, as a close does ([`Clew::send_then_close`]);
    /// then it goes on ([`Clew::decide_quit`]).
    fn send_then_quit(&mut self, agreed: HashSet<String>) -> Task<Shell> {
        let (dismissed, agreed) = self.drop_closes(agreed);
        for app in self.windows.values_mut() {
            app.flush_remote_edits();
        }
        let mut waits = false;
        for app in self.windows.values_mut() {
            if app.awaits_host(&agreed) {
                app.show_waiting(true);
                waits = true;
            }
        }
        let next = if waits {
            let ticket = self.ticket();
            self.quitting = Some(Pending::Confirming { ticket, agreed });
            self.wait_grace(ticket)
        } else {
            self.decide_quit(agreed)
        };
        dismissed.chain(next)
    }

    /// The hosts have answered what the quit sent, or the grace is over:
    /// clew quits — unless the windows would leave edits unsaved beyond
    /// `agreed`. Those are asked about, every window's in one question.
    fn decide_quit(&mut self, agreed: HashSet<String>) -> Task<Shell> {
        for app in self.windows.values_mut() {
            app.end_waiting();
        }
        let lost = self.everywhere(App::unsaved_edits);
        if covered(&agreed, &lost) {
            return self.quit_now();
        }
        self.ask_quit(lost, agreed)
    }

    /// Ask whether clew may quit and lose `lost` (a window's worth each) —
    /// and `agreed`, the edits the user agreed to lose already — in a sheet
    /// on the window the user is in. It takes the place of the windows' own
    /// questions, which [`Clew::send_then_quit`] withdrew.
    fn ask_quit(&mut self, lost: Vec<UnsavedEdits>, agreed: HashSet<String>) -> Task<Shell> {
        let present = |id: &window::Id| self.windows.contains_key(id);
        let Some(on) = self
            .focused
            .filter(present)
            .or_else(|| self.windows.keys().next().copied())
        else {
            // No window, so nothing to lose.
            return self.quit_now();
        };
        let (title, message) = crate::app::quit_question(&lost);
        let question = Question {
            title,
            message,
            keep: CANCEL_QUIT,
            lose: QUIT_ANYWAY,
        };
        let asked = Asked::new(self.ticket(), &lost, agreed);
        let by = if self.terminating {
            Asker::System
        } else {
            Asker::User
        };
        let task = self.ask_on(on, question, &asked, by);
        self.quitting = Some(Pending::Asking { on, asked });
        task
    }

    /// Drop every close under way — a quit takes them over — and return
    /// `agreed` with what their users agreed to lose added. The questions
    /// of those that ask are withdrawn, and their sheets ended: none stays up
    /// beside the quit's, none is begun after it, and no window is closed
    /// under one.
    fn drop_closes(&mut self, mut agreed: HashSet<String>) -> (Task<Shell>, HashSet<String>) {
        let mut dismissed = Vec::new();
        for (id, close) in self.closing.drain() {
            match close.pending {
                Pending::Confirming { agreed: theirs, .. } => agreed.extend(theirs),
                Pending::Asking { asked, .. } => {
                    asked.withdraw();
                    agreed.extend(asked.agreed);
                    dismissed.push(self.platform.dismiss(id).discard());
                }
            }
        }
        (Task::batch(dismissed), agreed)
    }

    /// The user answered question `ticket`. A window's: it closes — once its
    /// host has confirmed the rest — when they chose to lose what it named,
    /// and stays otherwise. A quit's: it goes on the same way when they
    /// chose to lose what it named, and is cancelled otherwise, and so is
    /// the terminate macOS asked for. An edit made while the question was up
    /// was not in it, so it is asked about in a new one rather than lost
    /// unasked. Kept, the edits given up that it named are not named again.
    fn answered(&mut self, ticket: u64, lose: bool) -> Task<Shell> {
        let window = self
            .closing
            .iter()
            .find(|(_, close)| close.pending.asks(ticket))
            .map(|(id, _)| *id);
        if let Some(id) = window
            && let Some(Close {
                pending: Pending::Asking { asked, .. },
                by,
            }) = self.closing.remove(&id)
        {
            if lose {
                return self.send_then_close(id, by, asked.lost());
            }
            // Kept: its transport sends the edits once it is back.
            if let Some(app) = self.windows.get_mut(&id) {
                app.forget_lost_edits(&asked.named);
            }
            return Task::none();
        }
        if self.quitting.as_ref().is_some_and(|q| q.asks(ticket))
            && let Some(Pending::Asking { asked, .. }) = self.quitting.take()
        {
            if lose {
                return self.send_then_quit(asked.lost());
            }
            for app in self.windows.values_mut() {
                app.forget_lost_edits(&asked.named);
            }
            return self.quit_cancelled();
        }
        // A question withdrawn since: its window went, a quit took its
        // place, or what it named was saved.
        Task::none()
    }

    /// The quit was cancelled. When macOS asked for it, AppKit is told clew
    /// stays; and the windows asked to close while it went on close as
    /// asked, now that it does not take them.
    fn quit_cancelled(&mut self) -> Task<Shell> {
        if std::mem::take(&mut self.terminating) {
            self.platform.reply_terminate(false);
        }
        let held: Vec<(window::Id, Asker)> = self.held_closes.drain().collect();
        let closes: Vec<Task<Shell>> = held
            .into_iter()
            .map(|(id, by)| self.request_close(id, by))
            .collect();
        Task::batch(closes)
    }

    /// Tear every window down and close it, then exit once the teardowns
    /// are done — the user agreed, or there was nothing to ask.
    ///
    /// The windows go here, rather than by closing each one and waiting for
    /// its `Closed`: an exit that only happens once the window map empties
    /// is an exit that never happens if one close is dropped, and a quit
    /// that can hang is worse than the leak it was meant to fix.
    fn quit_now(&mut self) -> Task<Shell> {
        // The windows asked to close meanwhile go with the rest.
        self.held_closes.clear();
        let ids: Vec<window::Id> = self.windows.keys().copied().collect();
        let closed: Vec<Task<Shell>> = ids.into_iter().map(|id| self.close_window(id)).collect();
        Task::batch([Task::batch(closed), self.exit_if_done()])
    }

    /// Tear window `id` down and close it, now: its `App` goes, so the
    /// window is not usable for the moment it stays on screen.
    ///
    /// Dropping the `App` is NOT enough to end the work the window started.
    /// Its debug adapter — and the debuggee that adapter launched — are
    /// processes owned by the startup stream, which iced keeps draining from
    /// the daemon's runtime after the window is gone; the stream's only
    /// cancellation seam is the run counter `on_window_closed` bumps. Without
    /// the teardown a locally spawned adapter (dlv, js-debug, or any adapter
    /// when no clew-server resolved) kept running with no window left to stop
    /// it.
    ///
    /// Every sheet on the window is ended before it closes — a question's,
    /// and a file picker's, which is a sheet on the window it was opened
    /// from: AppKit is left none to show, or finish, on a window that is
    /// gone. A question on it goes with it, withdrawn; a quit's is asked
    /// again, on a window still there, if one still has edits to ask about.
    fn close_window(&mut self, id: window::Id) -> Task<Shell> {
        let Some(mut app) = self.windows.remove(&id) else {
            return Task::none();
        };
        if self.focused == Some(id) {
            self.focused = self.windows.keys().next().copied();
        }
        self.held_closes.remove(&id);
        if let Some(Close {
            pending: Pending::Asking { asked, .. },
            ..
        }) = self.closing.remove(&id)
        {
            asked.withdraw();
        }
        let requit = match self.quitting.take() {
            Some(Pending::Asking { on, asked }) if on == id => {
                asked.withdraw();
                Some(asked.agreed)
            }
            quitting => {
                self.quitting = quitting;
                None
            }
        };
        let teardown = app
            .on_window_closed()
            .teardown
            .map(move |m| Shell::Window(id, m));
        let close = self
            .platform
            .dismiss(id)
            .discard()
            .chain(iced::window::close(id));
        let closed = Task::batch([close, self.track(teardown)]);
        match requit {
            Some(agreed) => Task::batch([closed, self.send_then_quit(agreed)]),
            None => closed,
        }
    }

    /// Run `teardown` under a ticket the exit waits for
    /// ([`Clew::exit_if_done`]): done once it has finished and every
    /// clew-server has been reaped, or once its grace is over, whichever
    /// comes first.
    ///
    /// `chain`, not `Task::batch`: a batch is a `SelectAll`, which sequences
    /// nothing, so an exit batched with the teardown won every time — the
    /// teardown needs a request and a response — and abandoned the adapter.
    /// The servers are waited for the same way: the windows released their
    /// transports (`App::on_window_closed`), each server has seen EOF, and
    /// its grace runs as a task on this runtime — exiting at once shut the
    /// runtime down under it, and every server was SIGKILLed a moment after
    /// its EOF, before reaping its language servers. The grace is the
    /// counterweight: nothing may make quitting depend on an adapter
    /// answering or a server exiting.
    fn track(&mut self, teardown: Task<Shell>) -> Task<Shell> {
        let ticket = self.ticket();
        self.tearing_down.insert(ticket);
        let reaped = self.platform.servers_reaped();
        let grace = self.platform.grace();
        Task::batch([
            teardown
                .chain(Task::future(reaped).discard())
                .chain(Task::done(Shell::TornDown(ticket))),
            // Built inside the future so no timer is armed (and no runtime is
            // required) until this actually runs.
            Task::future(async move { tokio::time::sleep(grace).await })
                .discard()
                .chain(Task::done(Shell::TornDown(ticket))),
        ])
    }

    /// Teardown `ticket` is over.
    fn torn_down(&mut self, ticket: u64) -> Task<Shell> {
        // Its other arm — the grace, or the teardown — came first.
        if !self.tearing_down.remove(&ticket) {
            return Task::none();
        }
        self.exit_if_done()
    }

    /// Exit, once nothing is left to wait for: no window, and no teardown
    /// still running. A window opened meanwhile — New Window works from the
    /// menu with none open — keeps clew running, and so does one a quit's
    /// teardown did not see coming: when macOS asked for that quit, AppKit is
    /// told clew stays. An exit macOS asked for is AppKit's to carry out, and
    /// it is answered instead.
    fn exit_if_done(&mut self) -> Task<Shell> {
        if self.exiting || !self.tearing_down.is_empty() {
            return Task::none();
        }
        if !self.windows.is_empty() {
            if self.terminating && self.quitting.is_none() {
                return self.quit_cancelled();
            }
            return Task::none();
        }
        self.exiting = true;
        if std::mem::take(&mut self.terminating) {
            self.platform.reply_terminate(true);
            return Task::none();
        }
        iced::exit()
    }

    /// macOS asked to terminate: quit, and tell AppKit how that ends. A quit
    /// under way already takes it over — its outcome is AppKit's.
    fn terminate(&mut self) -> Task<Shell> {
        if self.exiting {
            // The exit is on its way: AppKit may as well carry it out.
            self.platform.reply_terminate(true);
            return Task::none();
        }
        self.terminating = true;
        self.quit()
    }
}

pub fn update(clew: &mut Clew, message: Shell) -> Task<Shell> {
    match message {
        Shell::Window(id, msg) => {
            // Track focus so the menu / global actions hit the right window.
            if matches!(msg, Message::Window(WindowMsg::FocusChanged(true))) {
                clew.focused = Some(id);
            }
            // The window's close control and ⌘W ask the shell, which asks
            // about the edits the window would leave unsaved before it lets
            // it go.
            if matches!(msg, Message::Window(WindowMsg::Close)) {
                return clew.request_close(id, Asker::User);
            }
            // The update helper is installed and waits for this PROCESS to
            // exit, so the quit that follows is the whole app's: every window's
            // teardown, exactly like ⌘Q. Quitting from inside the installing
            // window used to tear down that window alone and leave the other
            // windows' debug adapters and debuggees running. This holds even
            // when the installing window has closed since — the helper is
            // waiting either way. Like ⌘Q, it asks first when a window holds
            // edits it cannot send; the helper waits a minute for the exit
            // (`EXIT_WAIT_TICKS`), and swaps nothing if the quit is cancelled
            // or comes later.
            if matches!(msg, Message::Updater(UpdaterMsg::Installed(Ok(())))) {
                let recorded = match clew.windows.get_mut(&id) {
                    Some(app) => app.update(msg).map(move |m| Shell::Window(id, m)),
                    None => Task::none(),
                };
                return Task::batch([recorded, update(clew, Shell::Quit)]);
            }
            // The appearance palette is process-global while `App`s are not,
            // so one window changing it repaints all of them — but only the
            // acting window learns of it. The others kept a stale preference
            // and, worse, kept their cached diagram SVGs in the previous
            // colors indefinitely. Watch the global revision across this
            // window's update and tell the rest to catch up.
            let before = crate::theme::revision();
            // Same for the keymap: a rebind lands in the acting window's copy.
            let keymap_before = clew.windows.get(&id).map(|app| app.keymap.stamp());
            let task = match clew.windows.get_mut(&id) {
                Some(app) => app.update(msg).map(move |m| Shell::Window(id, m)),
                // The window is gone; its async work is not. iced keeps
                // draining the streams it started. Its update download was
                // aborted as it closed (`App::on_window_closed`), but a result
                // already on its way still lands here, with nobody left to
                // install it — a whole release image, in a per-attempt
                // directory nothing else ever visits. Dropping the message has
                // to mean dropping the bytes too.
                None => {
                    if let Message::Updater(UpdaterMsg::Downloaded {
                        result: Ok(dmg), ..
                    }) = &msg
                    {
                        crate::updater::discard_download(dmg);
                    }
                    Task::none()
                }
            };
            let keymap_after = clew.windows.get(&id).map(|app| app.keymap.stamp());
            if keymap_before.is_some() && keymap_after != keymap_before {
                adopt_keymap(clew, id);
            }
            let task = if crate::theme::revision() == before {
                task
            } else {
                let resync: Vec<Task<Shell>> = clew
                    .windows
                    .keys()
                    .copied()
                    .filter(|other| *other != id)
                    .map(|other| {
                        Task::done(Shell::Window(
                            other,
                            Message::Settings(SettingsMsg::ThemeResynced),
                        ))
                    })
                    .collect();
                Task::batch([task, Task::batch(resync)])
            };
            // It may have been the host's answer a close or a quit waits on.
            Task::batch([task, clew.settle(id)])
        }
        Shell::ToFocused(msg) => match clew.focused {
            Some(id) => update(clew, Shell::Window(id, msg)),
            None => Task::none(),
        },
        Shell::NewWindow => clew.open_window(false),
        Shell::Opened(id) => {
            // Strip the OS chrome to the frameless look and round the corners,
            // and install the native menu bar (both main-thread; the menu once,
            // from the keymap — later keymap changes rebuild it, see
            // `adopt_keymap`), and the answer to macOS's terminate requests.
            #[cfg(target_os = "macos")]
            {
                crate::macos::configure_frameless(10.0);
                if let Some(app) = clew.windows.get(&id) {
                    crate::macos::menu::install_once(&app.keymap);
                }
                crate::macos::appearance::install_once();
                crate::macos::terminate::install_once();
            }
            #[cfg(not(target_os = "macos"))]
            let _ = id;
            Task::none()
        }
        Shell::CloseRequested(id) => clew.request_close(id, Asker::Outside),
        Shell::Closed(id) => clew.close_window(id),
        // ⌘Q used to reach AppKit's `terminate:`, which calls `exit(0)` from
        // inside `-[NSApplication run]`: no window ever emitted `Closed`, so no
        // window's teardown ran, and no Rust destructor ran either — the debug
        // adapter and the program it launched were simply abandoned. Quit is
        // now the same teardown a window's close does, once per window, with
        // the exit sequenced after all of them.
        Shell::Quit => clew.quit(),
        Shell::Terminate => clew.terminate(),
        Shell::Answered { ticket, lose } => clew.answered(ticket, lose),
        Shell::Waited(ticket) => clew.waited(ticket),
        Shell::TornDown(ticket) => clew.torn_down(ticket),
    }
}

/// Per-window view: render that window's `App`, tagging its messages with the
/// window id. A named `fn` (not a closure) so the borrow's higher-ranked
/// lifetime checks under the daemon's `ViewFn`.
pub fn view(clew: &Clew, id: window::Id) -> Element<'_, Shell> {
    match clew.windows.get(&id) {
        Some(app) => app.view().map(move |m| Shell::Window(id, m)),
        None => iced::widget::text("").into(),
    }
}

pub fn title(clew: &Clew, id: window::Id) -> String {
    clew.windows
        .get(&id)
        .map(App::title)
        .unwrap_or_else(|| "clew".to_string())
}

pub fn theme(clew: &Clew, id: window::Id) -> Theme {
    clew.windows.get(&id).map(App::theme).unwrap_or(Theme::Dark)
}

/// Hand the keymap `from` just changed to every other window, and rebuild the
/// menu bar from it.
///
/// Rebinding writes the global config, but each window dispatches with its own
/// in-memory copy, loaded when it opened: before this, the other windows kept
/// firing the old chords until they were reopened, and the menu — built once —
/// kept owning the old key equivalents forever.
fn adopt_keymap(clew: &mut Clew, from: window::Id) {
    let Some(keymap) = clew.windows.get(&from).map(|app| app.keymap.clone()) else {
        return;
    };
    for (other, app) in clew.windows.iter_mut() {
        if *other != from {
            app.keymap = keymap.clone();
        }
    }
    #[cfg(target_os = "macos")]
    crate::macos::menu::rebuild(&keymap);
}

/// Whether a key press reaches the window's key handler.
///
/// `listen_with` also sees events a focused widget already handled
/// (`Status::Captured`), and forwarding those unconditionally meant every key
/// typed into a text field ALSO drove the handler behind it: `j` moved the
/// code caret, `za` folded, `g` then `i` jumped to an implementation while the
/// user typed "git" into the search box. A captured press is therefore only
/// forwarded when the app must see it even with a field focused:
///
/// - Escape, so an overlay closes while its own input has focus (the input
///   captured the key to drop its focus);
/// - a ⌘-chord that is not one of the field's own editing chords, so ⌘P, ⌘F, …
///   work from inside any field. ⌘C/⌘X/⌘V/⌘A/⌘Z and ⌘/⌥/⌃ with an arrow are the
///   field's (copy, caret and word motion): forwarding ⌥← ran Back and ⌘C
///   overwrote the clipboard with the code selection. This is the same line
///   the menu bar draws when it decides which chords it may take away from
///   text fields (`Chord::menu_safe`).
fn forward_key(
    key: &keyboard::Key,
    modifiers: keyboard::Modifiers,
    status: iced::event::Status,
) -> bool {
    match status {
        iced::event::Status::Ignored => true,
        iced::event::Status::Captured => {
            matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::Escape)
            ) || (modifiers.command()
                && crate::keymap::Chord::from_event(key, modifiers).is_some_and(|c| c.menu_safe()))
        }
    }
}

/// Route a global runtime event to the shell message for the window it
/// occurred in. A named `fn` because `listen_with` takes a non-capturing one.
fn route(event: iced::Event, status: iced::event::Status, window: window::Id) -> Option<Shell> {
    match event {
        iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            forward_key(&key, modifiers, status).then(|| {
                Shell::Window(
                    window,
                    Message::Editor(EditorMsg::KeyPressed(key, modifiers)),
                )
            })
        }
        iced::Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => Some(Shell::Window(
            window,
            Message::Editor(EditorMsg::ModifiersChanged(m)),
        )),
        iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) => {
            Some(Shell::Window(window, Message::Editor(EditorMsg::SelectEnd)))
        }
        iced::Event::Window(iced::window::Event::Resized(size)) => Some(Shell::Window(
            window,
            Message::Window(WindowMsg::Resized(size)),
        )),
        iced::Event::Window(iced::window::Event::Opened { .. }) => Some(Shell::Opened(window)),
        // A close the OS asks for only *asks* (iced is told to leave it to
        // us: `exit_on_close_request`), like ⌘W and clew's own red control.
        iced::Event::Window(iced::window::Event::CloseRequested) => {
            Some(Shell::CloseRequested(window))
        }
        iced::Event::Window(iced::window::Event::Closed) => Some(Shell::Closed(window)),
        iced::Event::Window(iced::window::Event::Focused) => Some(Shell::Window(
            window,
            Message::Window(WindowMsg::FocusChanged(true)),
        )),
        iced::Event::Window(iced::window::Event::Unfocused) => Some(Shell::Window(
            window,
            Message::Window(WindowMsg::FocusChanged(false)),
        )),
        _ => None,
    }
}

pub fn subscription(clew: &Clew) -> Subscription<Shell> {
    // Global input events, routed to the window they occurred in (`route`
    // decides which captured key presses still count).
    let events = iced::event::listen_with(route);

    let mut subs = vec![events];
    // Each window's own async subscriptions (its clew-server stream, refresh
    // tick), tagged with its window id. `with` (not a capturing `map`) carries
    // the id, since Subscription::map closures must not capture.
    for (&id, app) in &clew.windows {
        subs.push(
            app.window_subscription()
                .with(id)
                .map(|(id, m)| Shell::Window(id, m)),
        );
    }
    // The app-global menu bar (one, at the top of the screen): app commands go
    // to the focused window; New Window is handled by the shell.
    #[cfg(target_os = "macos")]
    subs.push(crate::macos::menu::subscription().map(shell_from_menu));
    // Live OS appearance changes (for the System theme) go to the focused window.
    #[cfg(target_os = "macos")]
    subs.push(crate::macos::appearance::subscription().map(Shell::ToFocused));
    // macOS's own requests to quit (the Dock, a log-out) are the shell's.
    #[cfg(target_os = "macos")]
    subs.push(crate::macos::terminate::subscription().map(|()| Shell::Terminate));

    Subscription::batch(subs)
}

/// Translate an app-global menu command into a shell message.
#[cfg(target_os = "macos")]
fn shell_from_menu(cmd: crate::macos::menu::MenuCmd) -> Shell {
    use crate::macos::menu::MenuCmd;
    match cmd {
        MenuCmd::App(m) => Shell::ToFocused(m),
        MenuCmd::NewWindow => Shell::NewWindow,
        MenuCmd::Quit => Shell::Quit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReadingMsg;
    use crate::app::tests::{
        blank_app, data_dir_override, host_confirms, host_confirms_one, host_fails_one,
        remote_app_with_unanswered_edits, remote_app_with_unsent_edits, take_room, test_dir,
    };
    use crate::{DebugSession, DebugStatus};
    use iced::futures::StreamExt;
    use iced::futures::channel::oneshot;
    use iced::futures::future::Shared;
    use iced::futures::stream::BoxStream;
    use iced_test::runtime::Action;
    use iced_test::runtime::window::Action as WindowAction;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// What a test's platform was asked, and how the test answers it.
    #[derive(Default)]
    struct Script {
        /// Every question whose sheet was begun, in order ([`Sheet`]).
        sheets: Arc<Mutex<Vec<Sheet>>>,
        /// The answers AppKit was given to its terminate requests, in order.
        replies: Vec<bool>,
        /// When the clew-servers are reaped ([`Runtime::hold_reap`]); at once
        /// when `None`.
        reaped: Option<Shared<BoxFuture<'static, ()>>>,
        /// A teardown's grace, and a close's for its host; longer than any
        /// test when `None`.
        grace: Option<Duration>,
        /// What happened on screen, in the order it did — recorded as the
        /// platform's tasks run, and as windows close: what
        /// [`Runtime::check_sheets`] holds against AppKit's rules.
        steps: Arc<Mutex<Vec<Step>>>,
    }

    /// A question whose sheet was begun.
    struct Sheet {
        /// The window its sheet is on.
        on: window::Id,
        question: Question,
        /// The way to answer it, until it is answered or its sheet ended.
        answer: Option<oneshot::Sender<bool>>,
        /// Whether the shell withdrew it.
        withdrawn: Arc<AtomicBool>,
    }

    /// One thing that happened on screen.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Step {
        /// A window was brought where the user sees it, as far as who asked
        /// allows.
        Presented(window::Id, Asker),
        /// The `n`th question's sheet was begun on a window.
        Asked(window::Id, usize),
        /// The test answered the `n`th question.
        Answered(usize),
        /// The sheets on a window were ended.
        Dismissed(window::Id),
        /// A window was closed.
        Closed(window::Id),
    }

    /// A test's platform: nothing is shown and nothing reaches AppKit — all
    /// of it goes to the script the test holds.
    struct Scripted(Rc<RefCell<Script>>);

    impl Platform for Scripted {
        fn present(
            &self,
            window: window::Id,
            asker: Asker,
            withdrawn: Arc<AtomicBool>,
        ) -> Task<()> {
            let steps = self.0.borrow().steps.clone();
            Task::future(async move {
                if !withdrawn.load(Ordering::SeqCst) {
                    steps.lock().unwrap().push(Step::Presented(window, asker));
                }
            })
        }

        /// As the native sheet does: unless withdrawn by then, the sheets on
        /// the window end, and then this one is begun. Ending a question that
        /// still stands is the shell's mistake, and fails the test there.
        fn ask(
            &self,
            window: window::Id,
            question: Question,
            withdrawn: Arc<AtomicBool>,
        ) -> Task<bool> {
            let (sheets, steps) = {
                let script = self.0.borrow();
                (script.sheets.clone(), script.steps.clone())
            };
            let (answer, answered) = oneshot::channel();
            Task::future(async move {
                if withdrawn.load(Ordering::SeqCst) {
                    return None;
                }
                let mut sheets = sheets.lock().unwrap();
                let mut steps = steps.lock().unwrap();
                let n = sheets.len();
                let mut ended = false;
                for (up, sheet) in sheets.iter_mut().enumerate() {
                    if sheet.on == window
                        && let Some(answer) = sheet.answer.take()
                    {
                        assert!(
                            sheet.withdrawn.load(Ordering::SeqCst),
                            "question {up} still stood when question {n} ended it"
                        );
                        let _ = answer.send(false);
                        ended = true;
                    }
                }
                if ended {
                    steps.push(Step::Dismissed(window));
                }
                steps.push(Step::Asked(window, n));
                sheets.push(Sheet {
                    on: window,
                    question,
                    answer: Some(answer),
                    withdrawn,
                });
                Some(answered)
            })
            .then(|answered| match answered {
                Some(answered) => Task::future(async move { answered.await.unwrap_or(false) }),
                None => Task::done(false),
            })
        }

        /// As AppKit does: every sheet on the window ends, each answered as
        /// if cancelled.
        fn dismiss(&self, window: window::Id) -> Task<()> {
            let (sheets, steps) = {
                let script = self.0.borrow();
                (script.sheets.clone(), script.steps.clone())
            };
            Task::future(async move {
                for sheet in sheets.lock().unwrap().iter_mut() {
                    if sheet.on == window
                        && let Some(answer) = sheet.answer.take()
                    {
                        let _ = answer.send(false);
                    }
                }
                steps.lock().unwrap().push(Step::Dismissed(window));
            })
        }

        fn servers_reaped(&self) -> BoxFuture<'static, ()> {
            match &self.0.borrow().reaped {
                Some(reaped) => reaped.clone().boxed(),
                None => std::future::ready(()).boxed(),
            }
        }

        fn grace(&self) -> Duration {
            self.0.borrow().grace.unwrap_or(Duration::from_secs(3600))
        }

        fn reply_terminate(&self, terminate: bool) {
            self.0.borrow_mut().replies.push(terminate);
        }
    }

    /// A shell on a test's platform, whose questions are never answered.
    fn test_clew() -> Clew {
        Clew::new(Box::new(Scripted(Rc::default())))
    }

    /// A task [`Runtime`] runs, and whether it has woken since it was last
    /// polled.
    struct Running {
        stream: BoxStream<'static, Action<Shell>>,
        woken: Arc<Woken>,
    }

    /// Set when the task it wakes may go on.
    struct Woken(AtomicBool);

    impl iced::futures::task::ArcWake for Woken {
        fn wake_by_ref(woken: &Arc<Self>) {
            woken.0.store(true, Ordering::SeqCst);
        }
    }

    /// The daemon's runtime, as far as the shell's tests need one: the tasks
    /// `update` returns are run, and each action they take performed — a
    /// message delivered back to `update`, a window opened, or closed (and
    /// its `Closed` event delivered), an exit recorded. Every exit is checked
    /// as it is issued: no window left on screen or in the shell, and no
    /// question unanswered. And what reached the screen is checked against
    /// AppKit's rules for sheets ([`Runtime::check_sheets`]).
    struct Runtime {
        clew: Clew,
        script: Rc<RefCell<Script>>,
        /// The windows on screen.
        open: HashSet<window::Id>,
        /// The tasks still running: a question not answered, a grace not over.
        running: Vec<Running>,
        /// The exits iced was asked for.
        exits: usize,
        /// The windows a callback was asked to run on (`window::run`): where
        /// clew's own sheets are begun. None runs here.
        callbacks: Vec<window::Id>,
    }

    impl Runtime {
        fn new() -> Runtime {
            let script = Rc::new(RefCell::new(Script::default()));
            Runtime::on(Box::new(Scripted(script.clone())), script)
        }

        /// A runtime for a shell on `platform`, whose script is `script`.
        fn on(platform: Box<dyn Platform>, script: Rc<RefCell<Script>>) -> Runtime {
            Runtime {
                clew: Clew::new(platform),
                script,
                open: HashSet::new(),
                running: Vec::new(),
                exits: 0,
                callbacks: Vec::new(),
            }
        }

        /// A window on screen, with `app` in it, the one the user is in.
        fn window(&mut self, mut app: App) -> window::Id {
            let id = window::Id::unique();
            app.main_window = Some(id);
            self.clew.windows.insert(id, app);
            self.clew.focused = Some(id);
            self.open.insert(id);
            id
        }

        /// Hold the clew-servers' reaping until the sender returned fires.
        fn hold_reap(&mut self) -> oneshot::Sender<()> {
            let (reap, reaped) = oneshot::channel::<()>();
            self.script.borrow_mut().reaped = Some(reaped.map(|_| ()).boxed().shared());
            reap
        }

        /// Deliver `message`, and run what follows as far as it goes
        /// without the user (or the clock).
        fn send(&mut self, message: Shell) {
            self.deliver(message);
            self.settle();
        }

        /// `update`, and the task it returns set running. An exit AppKit is
        /// told to carry out is checked like one iced is asked for.
        fn deliver(&mut self, message: Shell) {
            let replies = self.script.borrow().replies.len();
            let task = update(&mut self.clew, message);
            if self.script.borrow().replies[replies..].contains(&true) {
                self.check_exit();
            }
            self.run(task);
        }

        /// An exit is being issued: nothing may be on screen, in the shell,
        /// or waiting for an answer.
        fn check_exit(&self) {
            assert!(
                self.open.is_empty(),
                "an exit with windows on screen: {:?}",
                self.open
            );
            assert!(
                self.clew.windows.is_empty(),
                "an exit with windows in the shell"
            );
            assert!(
                self.clew.closing.is_empty() && self.clew.quitting.is_none(),
                "an exit with a question unanswered"
            );
        }

        fn run(&mut self, task: Task<Shell>) {
            if let Some(stream) = iced_test::runtime::task::into_stream(task) {
                self.running.push(Running {
                    stream,
                    woken: Arc::new(Woken(AtomicBool::new(true))),
                });
            }
        }

        /// Run the tasks until none can go on: each is polled again once it
        /// has woken — at once, for the yield every iced task starts with; on
        /// an answer, a reap or a timer, for the rest.
        fn settle(&mut self) {
            for _ in 0..100_000 {
                let Some(next) = self
                    .running
                    .iter()
                    .position(|task| task.woken.0.swap(false, Ordering::SeqCst))
                else {
                    self.check_sheets();
                    return;
                };
                let mut task = self.running.swap_remove(next);
                let waker = iced::futures::task::waker(task.woken.clone());
                match task
                    .stream
                    .poll_next_unpin(&mut std::task::Context::from_waker(&waker))
                {
                    std::task::Poll::Ready(Some(action)) => {
                        task.woken.0.store(true, Ordering::SeqCst);
                        self.running.push(task);
                        self.perform(action);
                    }
                    std::task::Poll::Ready(None) => {}
                    std::task::Poll::Pending => self.running.push(task),
                }
            }
            panic!("the shell's tasks never settle");
        }

        fn perform(&mut self, action: Action<Shell>) {
            match action {
                Action::Output(message) => self.deliver(message),
                Action::Window(WindowAction::Open(id, _, opened)) => {
                    self.open.insert(id);
                    let _ = opened.send(id);
                }
                // Dropped unrun, as iced does for a window it no longer has.
                Action::Window(WindowAction::Run(id, _)) => self.callbacks.push(id),
                Action::Window(WindowAction::Close(id)) => {
                    self.step(Step::Closed(id));
                    // iced delivers the window's `Closed` event, once.
                    let was_open = self.open.remove(&id);
                    if was_open {
                        self.deliver(Shell::Closed(id));
                    }
                }
                Action::Exit => {
                    self.check_exit();
                    self.exits += 1;
                }
                _ => {}
            }
        }

        /// Record `step`.
        fn step(&self, step: Step) {
            self.script.borrow().steps.lock().unwrap().push(step);
        }

        /// What happened on screen so far.
        fn steps(&self) -> Vec<Step> {
            self.script.borrow().steps.lock().unwrap().clone()
        }

        /// What reached the screen, held against what AppKit does with
        /// sheets: a question is begun on a window brought where the user
        /// sees it first, never queued behind another question's sheet on
        /// the same window, and no window is closed with a sheet on it — an
        /// answer, or ending the window's sheets, takes a sheet down. Nor is
        /// a window closed without its sheets ended first: a file picker's
        /// sheet on it is not the shell's to see.
        fn check_sheets(&self) {
            let mut up: HashMap<window::Id, Vec<usize>> = HashMap::new();
            let mut presented: HashSet<window::Id> = HashSet::new();
            let mut ended: HashSet<window::Id> = HashSet::new();
            for step in self.steps() {
                match step {
                    Step::Presented(on, _) => {
                        presented.insert(on);
                    }
                    Step::Asked(on, n) => {
                        assert!(
                            presented.remove(&on),
                            "question {n} was begun on a window nobody may see"
                        );
                        let sheets = up.entry(on).or_default();
                        assert!(
                            sheets.is_empty(),
                            "question {n} was queued behind question {sheets:?} on its window"
                        );
                        sheets.push(n);
                        ended.remove(&on);
                    }
                    Step::Answered(n) => {
                        for sheets in up.values_mut() {
                            sheets.retain(|up| *up != n);
                        }
                    }
                    Step::Dismissed(on) => {
                        up.remove(&on);
                        ended.insert(on);
                    }
                    Step::Closed(on) => {
                        let sheets = up.remove(&on).unwrap_or_default();
                        assert!(
                            sheets.is_empty(),
                            "a window was closed under question {sheets:?}"
                        );
                        assert!(
                            ended.remove(&on),
                            "a window was closed without its sheets ended first"
                        );
                    }
                }
            }
        }

        /// Answer the question asked `n`th — `lose` its edits, or keep them —
        /// and run what follows.
        fn answer(&mut self, n: usize, lose: bool) {
            let answer = self.script.borrow().sheets.lock().unwrap()[n]
                .answer
                .take()
                .expect("answered already, or its sheet ended");
            self.step(Step::Answered(n));
            answer.send(lose).unwrap();
            self.settle();
        }

        /// The questions asked so far, each with the window its sheet is on.
        fn asked(&self) -> Vec<(window::Id, Question)> {
            self.script
                .borrow()
                .sheets
                .lock()
                .unwrap()
                .iter()
                .map(|sheet| (sheet.on, sheet.question.clone()))
                .collect()
        }

        /// Whether question `n`'s sheet is still up: not answered, and not
        /// ended.
        fn is_up(&self, n: usize) -> bool {
            self.script.borrow().sheets.lock().unwrap()[n]
                .answer
                .is_some()
        }

        /// Whether window `id` is on screen and in the shell.
        fn is_open(&self, id: window::Id) -> bool {
            self.open.contains(&id) && self.clew.windows.contains_key(&id)
        }
    }

    /// An edit in the window the user is in — the bookmark at `rel:line`
    /// removed — delivered the way the menu delivers its commands: to that
    /// window, its sheet up or not. Its journal holds the edit until a
    /// transport takes it.
    fn remove_bookmark(rel: &str, line: usize) -> Shell {
        Shell::ToFocused(Message::Reading(ReadingMsg::BookmarkRemoved {
            rel: rel.into(),
            line,
        }))
    }

    /// A window whose journal holds edits no transport can take asks before
    /// it closes — in a sheet on itself, saying how many edits to what, in
    /// which project — and nothing is torn down while it asks. Keep Window
    /// Open, the default, keeps the window and its edits for a transport
    /// that comes back to send, and it asks again the next time. Its close
    /// control, ⌘W and the OS all ask. (The edits used to be lost with the
    /// window, and a notice said so once it was gone.)
    #[tokio::test]
    async fn a_window_with_unsent_edits_asks_before_it_closes_and_may_be_kept() {
        let mut rt = Runtime::new();
        let (app, project) = remote_app_with_unsent_edits("shell-close-keep", 2);
        let window = rt.window(app);
        let live = rt.clew.windows[&window].debug_run_live.clone();
        let before = live.load(Ordering::SeqCst);

        rt.send(Shell::Window(window, Message::Window(WindowMsg::Close)));
        let asked = rt.asked();
        let [(on, question)] = asked.as_slice() else {
            panic!("one question: {asked:?}");
        };
        assert_eq!(*on, window, "the sheet is on the window closing");
        assert_eq!(question.title, "2 changes are not saved to the host");
        assert!(
            question
                .message
                .contains(&format!("2 changes to bookmarks in {project}")),
            "{}",
            question.message
        );
        assert_eq!(
            (question.keep, question.lose),
            ("Keep Window Open", "Close Anyway")
        );
        assert!(rt.is_open(window), "closed before the answer");
        assert_eq!(
            live.load(Ordering::SeqCst),
            before,
            "torn down before the answer"
        );
        // Asked again while it asks: its sheet is up already.
        rt.send(Shell::CloseRequested(window));
        assert_eq!(rt.asked().len(), 1);

        rt.answer(0, false);
        assert!(rt.is_open(window), "Keep Window Open closed it");
        assert_eq!(live.load(Ordering::SeqCst), before);
        let kept = rt.clew.windows[&window].unsaved_edits();
        assert_eq!(kept.map(|l| l.count), Some(2), "the edits went");
        assert_eq!(rt.exits, 0);

        // ⌘W, from the menu, and the OS ask the same.
        rt.send(Shell::ToFocused(Message::Window(WindowMsg::Close)));
        assert_eq!(rt.asked().len(), 2);
        rt.answer(1, false);
        rt.send(Shell::CloseRequested(window));
        assert_eq!(rt.asked().len(), 3);
        assert!(rt.is_open(window));
    }

    /// clew's own question is a sheet begun from the callback of the window
    /// it is about, which is brought forward first, from its callback too. A
    /// window gone before those callbacks run never gets it, and the answer
    /// is the default: the edits are kept.
    #[tokio::test]
    async fn a_sheet_is_begun_on_its_window_and_one_never_shown_keeps_the_edits() {
        let mut rt = Runtime::on(Box::new(Native), Rc::default());
        let (app, _) = remote_app_with_unsent_edits("shell-sheet-unshown", 1);
        let window = rt.window(app);
        rt.send(Shell::CloseRequested(window));
        assert_eq!(
            rt.callbacks,
            [window, window],
            "not brought forward, then a sheet, on the window closing"
        );
        assert!(rt.clew.closing.is_empty(), "never answered");
        assert!(rt.is_open(window), "closed on a question never shown");
        assert_eq!(rt.exits, 0);
    }

    /// Close Anyway tears the window down and closes it, at once: its debug
    /// run ends and its edits go. The last window closing exits clew once
    /// its teardown is over — with no question, and no notice, to wait for.
    #[tokio::test]
    async fn closing_anyway_tears_the_window_down_and_the_last_one_exits() {
        let mut rt = Runtime::new();
        let (mut app, _) = remote_app_with_unsent_edits("shell-close-anyway", 1);
        app.debug.session = Some(running_session());
        app.bump_debug_run();
        let live = app.debug_run_live.clone();
        let window = rt.window(app);
        let before = live.load(Ordering::SeqCst);

        rt.send(Shell::CloseRequested(window));
        assert_eq!(rt.asked().len(), 1);
        rt.answer(0, true);

        assert_ne!(
            live.load(Ordering::SeqCst),
            before,
            "the window's debug run outlived it"
        );
        assert!(!rt.open.contains(&window) && rt.clew.windows.is_empty());
        assert_eq!(rt.exits, 1, "the last window closed and clew stayed");
    }

    /// A window opened while the last one asks whether it may close keeps
    /// clew running once that one has closed. The exit used to be armed as
    /// the last window closed, and to fire once a notice was dismissed — and
    /// take a window opened meanwhile with it, its edits unreported and its
    /// debug adapter orphaned.
    #[tokio::test]
    async fn a_window_opened_while_a_close_is_asked_about_keeps_clew_running() {
        let mut rt = Runtime::new();
        let (app, _) = remote_app_with_unsent_edits("shell-close-opened", 1);
        let first = rt.window(app);
        rt.send(Shell::CloseRequested(first));
        rt.send(Shell::NewWindow);
        let second = rt.clew.focused.expect("the new window");
        assert!(second != first && rt.is_open(second));

        rt.answer(0, true);
        assert!(!rt.open.contains(&first));
        assert!(rt.is_open(second));
        assert_eq!(rt.exits, 0, "clew exited under the new window");

        // It has nothing to ask about: it goes at once, and clew with it.
        rt.send(Shell::CloseRequested(second));
        assert_eq!(rt.asked().len(), 1);
        assert_eq!(rt.exits, 1);
    }

    /// The exit waits for the last window's teardown — every clew-server
    /// reaped, or the grace over — and a window opened meanwhile (New Window
    /// works from the menu with none open) keeps clew running.
    #[tokio::test]
    async fn a_window_opened_while_the_last_one_tears_down_keeps_clew_running() {
        let mut rt = Runtime::new();
        let reap = rt.hold_reap();
        let first = rt.window(blank_app());
        rt.send(Shell::CloseRequested(first));
        assert!(rt.asked().is_empty());
        assert!(!rt.open.contains(&first));
        assert_eq!(rt.exits, 0, "exited with a server still alive");

        rt.send(Shell::NewWindow);
        reap.send(()).unwrap();
        rt.settle();
        let second = rt.clew.focused.expect("the new window");
        assert!(rt.is_open(second));
        assert_eq!(rt.exits, 0, "clew exited under the new window");

        rt.send(Shell::CloseRequested(second));
        assert_eq!(rt.exits, 1);
    }

    /// An exit waits for the servers to be reaped — not less: exiting at
    /// once shut the runtime down under the servers' grace tasks, and each
    /// was SIGKILLed right after its EOF, before it could reap its language
    /// servers — and no longer than the grace, whatever is still alive.
    #[tokio::test]
    async fn a_quit_exits_once_the_servers_are_reaped_or_the_grace_is_over() {
        let mut rt = Runtime::new();
        let reap = rt.hold_reap();
        rt.window(blank_app());
        rt.send(Shell::Quit);
        tokio::time::sleep(Duration::from_millis(300)).await;
        rt.settle();
        assert_eq!(rt.exits, 0, "the quit exited with a server still alive");
        reap.send(()).unwrap();
        rt.settle();
        assert_eq!(rt.exits, 1, "no exit once the servers were reaped");

        let mut rt = Runtime::new();
        let _never = rt.hold_reap();
        rt.script.borrow_mut().grace = Some(Duration::from_millis(200));
        rt.window(blank_app());
        rt.send(Shell::Quit);
        assert_eq!(rt.exits, 0);
        tokio::time::sleep(Duration::from_millis(400)).await;
        rt.settle();
        assert_eq!(rt.exits, 1, "a server that never exits held the quit");
    }

    /// ⌘Q asks once, before anything is torn down, naming every window that
    /// holds edits no transport can take — and only those — in a sheet on
    /// the window the user is in. Cancel ends the quit: every window stays,
    /// with its edits and its work.
    #[tokio::test]
    async fn a_quit_asks_once_for_every_window_and_cancel_keeps_them_all() {
        let mut rt = Runtime::new();
        let (first, first_project) = remote_app_with_unsent_edits("shell-quit-cancel-a", 2);
        let (third, third_project) = remote_app_with_unsent_edits("shell-quit-cancel-c", 1);
        let ids = [rt.window(first), rt.window(blank_app()), rt.window(third)];
        rt.clew.focused = Some(ids[1]);
        let lives: Vec<_> = ids
            .iter()
            .map(|id| rt.clew.windows[id].debug_run_live.clone())
            .collect();
        let before: Vec<u64> = lives.iter().map(|l| l.load(Ordering::SeqCst)).collect();
        let torn_down = |rt: &Runtime| {
            lives
                .iter()
                .zip(&before)
                .any(|(live, before)| live.load(Ordering::SeqCst) != *before)
                || ids.iter().any(|id| !rt.is_open(*id))
        };

        rt.send(Shell::Quit);
        let asked = rt.asked();
        let [(on, question)] = asked.as_slice() else {
            panic!("one question: {asked:?}");
        };
        assert_eq!(*on, ids[1], "the sheet is on the window the user is in");
        assert_eq!(question.title, "3 changes are not saved to their hosts");
        assert!(
            question
                .message
                .contains(&format!("• 2 changes to bookmarks in {first_project}"))
                && question
                    .message
                    .contains(&format!("• 1 change to bookmarks in {third_project}")),
            "{}",
            question.message
        );
        assert_eq!((question.keep, question.lose), ("Cancel", "Quit Anyway"));
        assert!(!torn_down(&rt), "torn down before the answer");
        // ⌘Q again while it asks: its sheet is up already.
        rt.send(Shell::Quit);
        assert_eq!(rt.asked().len(), 1);

        rt.answer(0, false);
        assert!(!torn_down(&rt), "a cancelled quit tore windows down");
        assert_eq!(rt.exits, 0);
        assert!(
            rt.script.borrow().replies.is_empty(),
            "AppKit was answered a question it never asked"
        );
    }

    /// Quit Anyway tears every window down and closes it, at once — a window
    /// opened while the question was up included — then exits, once.
    #[tokio::test]
    async fn a_confirmed_quit_tears_every_window_down_and_exits() {
        let mut rt = Runtime::new();
        let (first, _) = remote_app_with_unsent_edits("shell-quit-confirm", 1);
        rt.window(first);
        let mut second = blank_app();
        second.debug.session = Some(running_session());
        second.bump_debug_run();
        let live = second.debug_run_live.clone();
        rt.window(second);
        let before = live.load(Ordering::SeqCst);

        rt.send(Shell::Quit);
        rt.send(Shell::NewWindow);
        assert_eq!(rt.open.len(), 3);
        assert_eq!(rt.asked().len(), 1);
        rt.answer(0, true);

        assert_ne!(
            live.load(Ordering::SeqCst),
            before,
            "a window's debug run outlived the quit"
        );
        assert!(rt.open.is_empty() && rt.clew.windows.is_empty());
        assert_eq!(rt.exits, 1);
    }

    /// A quit takes the place of the questions windows ask before they
    /// close: their sheets are ended, and then its one question, naming
    /// every window's edits, is begun on the window the user is in. It used
    /// to be queued behind that window's own sheet — answered Close Anyway,
    /// that one closed the window just as AppKit began the quit's sheet on
    /// it. A window asking while the quit's question is up has its sheet
    /// ended before the quit closes it: no window is closed under a sheet.
    #[tokio::test]
    async fn a_quit_takes_the_place_of_the_windows_questions_and_closes_none_under_one() {
        let mut rt = Runtime::new();
        let (first, _) = remote_app_with_unsent_edits("shell-requit-a", 1);
        let (second, _) = remote_app_with_unsent_edits("shell-requit-b", 1);
        let (first, second) = (rt.window(first), rt.window(second));
        rt.send(Shell::CloseRequested(first));
        rt.send(Shell::CloseRequested(second));
        rt.clew.focused = Some(first);
        rt.send(Shell::Quit);
        let asked = rt.asked();
        assert_eq!(asked.len(), 3);
        assert_eq!(asked[2].0, first, "the quit's sheet is not with the user");
        assert_eq!(asked[2].1.title, "2 changes are not saved to their hosts");
        let steps = rt.steps();
        let quit_begun = steps
            .iter()
            .position(|step| *step == Step::Asked(first, 2))
            .expect("the quit's sheet was never begun");
        for window in [first, second] {
            let ended = steps
                .iter()
                .position(|step| *step == Step::Dismissed(window));
            assert!(
                ended.is_some_and(|ended| ended < quit_begun),
                "a window's question stayed up beside the quit's: {steps:?}"
            );
        }
        assert!(rt.clew.closing.is_empty(), "the windows' questions stand");

        rt.send(Shell::CloseRequested(second));
        assert_eq!(rt.asked().len(), 4);
        assert_eq!(rt.asked()[3].0, second);
        rt.answer(2, true);
        let steps = rt.steps();
        let ended = steps
            .iter()
            .rposition(|step| *step == Step::Dismissed(second))
            .expect("its sheet was never ended");
        let closed = steps
            .iter()
            .position(|step| *step == Step::Closed(second))
            .expect("the quit left it open");
        assert!(ended < closed, "it was closed under its sheet: {steps:?}");
        assert!(rt.open.is_empty());
        assert_eq!(rt.exits, 1);
    }

    /// An edit made while the question is up — the menu still reaches the
    /// window — was not in it: the answer to lose the edits then asks again,
    /// naming it, rather than losing it unasked. For a window's question and
    /// for a quit's.
    #[tokio::test]
    async fn an_edit_made_while_asking_is_asked_about_before_it_is_lost() {
        let mut rt = Runtime::new();
        let (app, _) = remote_app_with_unsent_edits("shell-close-reask", 1);
        let window = rt.window(app);
        rt.send(Shell::CloseRequested(window));
        rt.send(remove_bookmark("b.rs", 2));
        rt.answer(0, true);
        let asked = rt.asked();
        assert_eq!(asked.len(), 2, "the new edit was lost unasked");
        assert_eq!(asked[1].1.title, "2 changes are not saved to the host");
        assert!(rt.is_open(window));
        rt.answer(1, true);
        assert!(!rt.open.contains(&window));
        assert_eq!(rt.exits, 1);

        let mut rt = Runtime::new();
        let (app, _) = remote_app_with_unsent_edits("shell-quit-reask", 1);
        let window = rt.window(app);
        rt.send(Shell::Quit);
        rt.send(remove_bookmark("b.rs", 2));
        rt.answer(0, true);
        let asked = rt.asked();
        assert_eq!(asked.len(), 2, "the new edit was lost unasked");
        assert_eq!(asked[1].1.title, "2 changes are not saved to the host");
        assert!(rt.is_open(window));
        rt.answer(1, true);
        assert!(rt.open.is_empty());
        assert_eq!(rt.exits, 1);
    }

    /// macOS's own quit — the Dock's Quit item, a log-out — is a quit whose
    /// outcome AppKit is told: Cancel keeps clew running, and AppKit hears
    /// so; once agreed, every window is torn down, and then AppKit ends the
    /// process, not iced. With nothing to ask, it goes the same way; and a
    /// window opened while it tears down keeps clew running, which AppKit
    /// hears too.
    #[tokio::test]
    async fn macos_asking_to_terminate_is_a_quit_answered_to_appkit() {
        let mut rt = Runtime::new();
        let (app, _) = remote_app_with_unsent_edits("shell-terminate", 1);
        let window = rt.window(app);
        rt.send(Shell::Terminate);
        assert_eq!(rt.asked().len(), 1);
        rt.answer(0, false);
        assert_eq!(rt.script.borrow().replies, [false]);
        assert!(rt.is_open(window));

        rt.send(Shell::Terminate);
        rt.answer(1, true);
        assert!(rt.open.is_empty() && rt.clew.windows.is_empty());
        assert_eq!(rt.script.borrow().replies, [false, true]);
        assert_eq!(rt.exits, 0, "iced exited under AppKit's own termination");

        // Asked while ⌘Q's question is up: that question's answer is AppKit's.
        let mut rt = Runtime::new();
        let (app, _) = remote_app_with_unsent_edits("shell-terminate-quit", 1);
        rt.window(app);
        rt.send(Shell::Quit);
        rt.send(Shell::Terminate);
        assert_eq!(rt.asked().len(), 1);
        rt.answer(0, false);
        assert_eq!(rt.script.borrow().replies, [false]);

        // Nothing to ask.
        let mut rt = Runtime::new();
        rt.window(blank_app());
        rt.send(Shell::Terminate);
        assert!(rt.asked().is_empty());
        assert_eq!(rt.script.borrow().replies, [true]);
        assert_eq!(rt.exits, 0);

        // A window opened while the windows tear down.
        let mut rt = Runtime::new();
        let reap = rt.hold_reap();
        rt.window(blank_app());
        rt.send(Shell::Terminate);
        rt.send(Shell::NewWindow);
        reap.send(()).unwrap();
        rt.settle();
        assert_eq!(rt.script.borrow().replies, [false]);
        assert_eq!(rt.open.len(), 1);
        assert_eq!(rt.exits, 0);
    }

    /// Sent is not saved: a transport that died without anyone noticing —
    /// SSH notices after about 45 s — takes the edits a closing window sends
    /// into a pipe that goes nowhere, and they went with the window, unasked
    /// and unreported. The close now waits for its host to confirm them, up
    /// to the grace, the window open and nothing torn down meanwhile; then
    /// asks about what is unconfirmed, as sent and not confirmed. Keep
    /// Window Open keeps it all, and a host that answers in time closes the
    /// window without a question.
    #[tokio::test]
    async fn a_close_waits_for_its_host_to_confirm_the_edits_and_asks_when_it_does_not() {
        let mut rt = Runtime::new();
        rt.script.borrow_mut().grace = Some(Duration::from_millis(100));
        let (app, project, _outbox) = remote_app_with_unanswered_edits("shell-unconfirmed", 2);
        let window = rt.window(app);
        let live = rt.clew.windows[&window].debug_run_live.clone();
        let before = live.load(Ordering::SeqCst);

        rt.send(Shell::CloseRequested(window));
        assert!(rt.asked().is_empty(), "asked before the host could answer");
        assert!(rt.is_open(window), "closed with its edits unconfirmed");
        assert_eq!(live.load(Ordering::SeqCst), before, "torn down meanwhile");
        let sent = rt.clew.windows[&window].unconfirmed_edits();
        assert_eq!(sent.map(|l| l.count), Some(2), "the close sent nothing");

        tokio::time::sleep(Duration::from_millis(300)).await;
        rt.settle();
        let asked = rt.asked();
        let [(on, question)] = asked.as_slice() else {
            panic!("one question: {asked:?}");
        };
        assert_eq!(*on, window);
        assert_eq!(question.title, "2 changes are not confirmed by the host");
        assert!(
            question.message.contains(&format!(
                "2 changes to bookmarks in {project} were sent to the host, but it has not \
                 confirmed them"
            )),
            "{}",
            question.message
        );
        assert_eq!(
            (question.keep, question.lose),
            ("Keep Window Open", "Close Anyway")
        );
        assert!(rt.is_open(window));
        rt.answer(0, false);
        assert!(rt.is_open(window), "Keep Window Open closed it");
        assert_eq!(live.load(Ordering::SeqCst), before);
        let kept = rt.clew.windows[&window].unconfirmed_edits();
        assert_eq!(kept.map(|l| l.count), Some(2), "the edits went");

        rt.send(Shell::CloseRequested(window));
        for answer in host_confirms(&rt.clew.windows[&window]) {
            rt.send(Shell::Window(window, answer));
        }
        assert_eq!(rt.asked().len(), 1, "asked about edits the host confirmed");
        assert!(!rt.open.contains(&window), "the confirmed window stayed");
        assert_ne!(live.load(Ordering::SeqCst), before);
        assert_eq!(rt.exits, 1);
    }

    /// A quit waits for the hosts the same way — every window open, nothing
    /// torn down — and asks about what they have not confirmed, in one
    /// question, once the grace is over.
    #[tokio::test]
    async fn a_quit_waits_for_the_hosts_to_confirm_the_edits_and_asks_when_they_do_not() {
        let mut rt = Runtime::new();
        rt.script.borrow_mut().grace = Some(Duration::from_millis(100));
        let (app, project, _outbox) = remote_app_with_unanswered_edits("shell-quit-unconfirmed", 1);
        rt.window(app);
        rt.window(blank_app());
        rt.send(Shell::Quit);
        assert!(rt.asked().is_empty(), "asked before the host could answer");
        assert_eq!(rt.open.len(), 2, "quit with an edit unconfirmed");

        tokio::time::sleep(Duration::from_millis(300)).await;
        rt.settle();
        let asked = rt.asked();
        let [(_, question)] = asked.as_slice() else {
            panic!("one question: {asked:?}");
        };
        assert_eq!(question.title, "A change is not confirmed by the host");
        assert!(
            question.message.contains(&format!(
                "1 change to bookmarks in {project} was sent to the host, but it has not \
                 confirmed it"
            )),
            "{}",
            question.message
        );
        assert_eq!((question.keep, question.lose), ("Cancel", "Quit Anyway"));
        assert_eq!(rt.open.len(), 2);
        rt.answer(0, true);
        assert!(rt.open.is_empty());
        assert_eq!(rt.exits, 1);
    }

    /// A close whose window holds edits for every reason — the transport
    /// took all but one, and the host answers none — costs one wait and one
    /// question, which says how many there are for each reason. It used to
    /// ask about the edit it could not send before it sent the rest, and
    /// then, after the wait, about the rest: two questions, and a second
    /// wait when the answer sent more. Closed anyway, the window goes at
    /// once.
    #[tokio::test]
    async fn a_close_asks_once_about_edits_not_saved_for_every_reason() {
        let mut rt = Runtime::new();
        rt.script.borrow_mut().grace = Some(Duration::from_millis(100));
        let (app, project, _outbox) = remote_app_with_unanswered_edits("shell-close-merged", 3);
        take_room(&app, 1);
        let window = rt.window(app);
        rt.send(Shell::Window(window, Message::Window(WindowMsg::Close)));
        assert!(rt.asked().is_empty(), "asked before the host could answer");

        tokio::time::sleep(Duration::from_millis(300)).await;
        rt.settle();
        let asked = rt.asked();
        let [(_, question)] = asked.as_slice() else {
            panic!("one question: {asked:?}");
        };
        assert_eq!(question.title, "3 changes are not saved to the host");
        assert_eq!(
            question.message,
            format!(
                "3 changes to bookmarks in {project} are not saved to the host: 1 could not be \
                 sent and 2 were sent but not confirmed. Keep the window open, and clew keeps \
                 trying to save them. Close it, and they may be lost."
            )
        );
        // Asked after the grace, of a close the user asked for in clew.
        assert!(rt.steps().contains(&Step::Presented(window, Asker::User)));
        rt.answer(0, true);
        assert_eq!(rt.asked().len(), 1, "asked twice");
        assert!(!rt.open.contains(&window), "waited again");
        assert_eq!(rt.exits, 1);
    }

    /// The Dock's Quit, or a log-out, while clew is hidden (⌘H) or the
    /// window is minimized: the sheet was begun where nobody saw it, the quit
    /// seemed to hang, and a log-out ended with macOS saying clew interrupted
    /// it. Every question now brings its window where the user sees it
    /// first — macOS's, ⌘Q's and a window's own — as far as who asked
    /// allows: macOS waits on the answer, ⌘Q is the user's in clew, and a
    /// close the OS asked for may come from anywhere.
    #[tokio::test]
    async fn every_question_brings_its_window_where_the_user_sees_it_first() {
        let mut rt = Runtime::new();
        let (app, _) = remote_app_with_unsent_edits("shell-present", 1);
        let window = rt.window(app);
        rt.send(Shell::Terminate);
        rt.answer(0, false);
        rt.send(Shell::Quit);
        rt.answer(1, false);
        rt.send(Shell::CloseRequested(window));
        let seen: Vec<Step> = rt
            .steps()
            .into_iter()
            .filter(|step| matches!(step, Step::Presented(..) | Step::Asked(..)))
            .collect();
        assert_eq!(
            seen,
            [
                Step::Presented(window, Asker::System),
                Step::Asked(window, 0),
                Step::Presented(window, Asker::User),
                Step::Asked(window, 1),
                Step::Presented(window, Asker::Outside),
                Step::Asked(window, 2),
            ]
        );
    }

    /// A window that closes another way while its question is up — the OS
    /// closed it — has the sheet ended first, like every window closed with
    /// one: AppKit is left none to show, or finish, on a window gone.
    #[tokio::test]
    async fn a_window_is_never_closed_under_its_question() {
        let mut rt = Runtime::new();
        let (app, _) = remote_app_with_unsent_edits("shell-closed-asking", 1);
        let window = rt.window(app);
        rt.send(Shell::CloseRequested(window));
        rt.send(Shell::Closed(window));
        let steps = rt.steps();
        let ended = steps
            .iter()
            .position(|step| *step == Step::Dismissed(window));
        let closed = steps.iter().position(|step| *step == Step::Closed(window));
        assert!(
            ended.is_some() && ended < closed,
            "closed under its sheet: {steps:?}"
        );
        assert_eq!(rt.exits, 1);
    }

    /// The requests of the edits window `id` has on the wire, oldest first.
    fn on_the_wire(rt: &Runtime, id: window::Id) -> Vec<u64> {
        rt.clew.windows[&id]
            .proj
            .remote_edits
            .iter()
            .filter_map(|edit| edit.request)
            .collect()
    }

    /// The host answers request `request` of window `id`: it saved the edit,
    /// or — `failed` — it could not.
    fn host_answers(rt: &mut Runtime, id: window::Id, request: u64, failed: bool) {
        let app = &rt.clew.windows[&id];
        let answer = if failed {
            host_fails_one(app, request, "database is locked")
        } else {
            host_confirms_one(app, request)
        };
        rt.send(Shell::Window(id, answer));
    }

    /// The host fails an edit a close sent while an edit after it is on the
    /// wire too, and saves that one. The failed edit used to be dropped at
    /// once, and the window then closed without asking — the edit gone with
    /// a status line nobody saw. The two removals change different
    /// bookmarks, so the failed one is kept to be tried again, and the close
    /// asks about it once nothing it sent is left unanswered.
    #[tokio::test]
    async fn an_edit_the_host_fails_while_a_close_waits_is_asked_about() {
        let mut rt = Runtime::new();
        let (app, project, _outbox) = remote_app_with_unanswered_edits("shell-failed-wait", 2);
        let window = rt.window(app);
        rt.send(Shell::CloseRequested(window));
        let [first, second] = on_the_wire(&rt, window)[..] else {
            panic!("the close sent both");
        };
        host_answers(&mut rt, window, first, true);
        host_answers(&mut rt, window, second, false);
        let asked = rt.asked();
        let [(on, question)] = asked.as_slice() else {
            panic!("closed without asking about the edit the host failed: {asked:?}");
        };
        assert_eq!(*on, window);
        assert_eq!(question.title, "A change is not saved to the host");
        assert_eq!(
            question.message,
            format!(
                "1 change to bookmarks in {project} could not be saved by the host. Keep the \
                 window open, and it is tried again. Close it, and it is lost."
            )
        );
        assert!(rt.is_open(window));
    }

    /// When both fail, both are counted — the question used to say one
    /// change could not be sent, the other dropped unmentioned, when both
    /// had been sent — and both are kept to be tried again.
    #[tokio::test]
    async fn edits_the_host_fails_are_all_counted_as_what_they_are() {
        let mut rt = Runtime::new();
        let (app, project, _outbox) = remote_app_with_unanswered_edits("shell-both-failed", 2);
        let window = rt.window(app);
        rt.send(Shell::CloseRequested(window));
        let [first, second] = on_the_wire(&rt, window)[..] else {
            panic!("the close sent both");
        };
        host_answers(&mut rt, window, first, true);
        host_answers(&mut rt, window, second, true);
        let asked = rt.asked();
        let [(_, question)] = asked.as_slice() else {
            panic!("one question: {asked:?}");
        };
        assert_eq!(question.title, "2 changes are not saved to the host");
        assert!(
            question.message.starts_with(&format!(
                "2 changes to bookmarks in {project} could not be saved by the host."
            )),
            "{}",
            question.message
        );
        assert_eq!(rt.clew.windows[&window].proj.remote_edits.len(), 2);
    }

    /// When the edit after a failed one changes the same entry and lands,
    /// the failed one is given up — sent again, the older note would land
    /// over the newer. The close does not go unasked: its question names
    /// the change lost. Kept open, the window does not name it again.
    #[tokio::test]
    async fn a_change_given_up_while_a_close_waits_is_named_in_its_question() {
        let mut rt = Runtime::new();
        let (mut app, project, _outbox) = remote_app_with_unanswered_edits("shell-lost-wait", 0);
        for note in ["first", "second"] {
            let merge = crate::bookmarks::merge_note("a.rs", 1, Some(note.into()));
            assert!(app.edit_remote_state(crate::bookmarks::REL, merge));
        }
        let window = rt.window(app);
        rt.send(Shell::CloseRequested(window));
        let [first, second] = on_the_wire(&rt, window)[..] else {
            panic!("the close sent both");
        };
        host_answers(&mut rt, window, first, true);
        host_answers(&mut rt, window, second, false);
        let asked = rt.asked();
        let [(_, question)] = asked.as_slice() else {
            panic!("closed without naming the change it lost: {asked:?}");
        };
        assert_eq!(question.title, "A change is not saved to the host");
        assert_eq!(
            question.message,
            format!(
                "The host could not save the change to the bookmark at a.rs:1 in {project}: it \
                 is lost. Keep the window open to make it again, or close it."
            )
        );
        rt.answer(0, false);
        assert!(rt.is_open(window));
        rt.send(Shell::CloseRequested(window));
        assert_eq!(rt.asked().len(), 1, "named twice");
        assert!(!rt.open.contains(&window));
    }

    /// ⌘W and ⌘Q handled before the close's question is begun: the quit
    /// takes the close's place, and the close's question — withdrawn — is
    /// never begun. It used to be, after the quit had ended the window's
    /// sheets: a stale question whose answer nobody read, with the quit's
    /// queued behind it.
    #[tokio::test]
    async fn a_close_question_a_quit_took_the_place_of_is_never_begun() {
        let mut rt = Runtime::new();
        let (app, _) = remote_app_with_unsent_edits("shell-close-then-quit", 1);
        let window = rt.window(app);
        rt.deliver(Shell::CloseRequested(window));
        rt.deliver(Shell::Quit);
        rt.settle();
        let asked = rt.asked();
        let [(on, question)] = asked.as_slice() else {
            panic!("the close's question was begun as well: {asked:?}");
        };
        assert_eq!((*on, question.keep), (window, CANCEL_QUIT));
        assert!(
            !rt.steps()
                .contains(&Step::Presented(window, Asker::Outside))
        );
        rt.answer(0, true);
        assert_eq!(rt.exits, 1);
    }

    /// The window the quit's question is on closes another way — the OS
    /// closed it: the question goes with it, and is asked again on a window
    /// still there. Left standing, it held every later ⌘Q, and macOS's own
    /// quit, for good.
    #[tokio::test]
    async fn a_quit_whose_window_closes_asks_again_on_another() {
        let mut rt = Runtime::new();
        let (first, _) = remote_app_with_unsent_edits("shell-requit-a", 1);
        let (second, _) = remote_app_with_unsent_edits("shell-requit-b", 1);
        let (first, second) = (rt.window(first), rt.window(second));
        rt.clew.focused = Some(first);
        rt.send(Shell::Quit);
        assert_eq!(rt.asked().len(), 1);
        rt.open.remove(&first);
        rt.send(Shell::Closed(first));
        let asked = rt.asked();
        assert_eq!(asked.len(), 2, "the quit's question went with its window");
        assert_eq!(asked[1].0, second);
        assert_eq!(asked[1].1.title, "A change is not saved to the host");
        rt.answer(1, true);
        assert!(rt.open.is_empty());
        assert_eq!(rt.exits, 1);
    }

    /// A question about edits the host had not confirmed no longer stands
    /// once the host confirms them: it is withdrawn, its sheet ended, and
    /// the window goes — it used to stay up, and Keep then cancelled a close
    /// that could have gone.
    #[tokio::test]
    async fn a_question_the_host_answers_meanwhile_is_withdrawn() {
        let mut rt = Runtime::new();
        rt.script.borrow_mut().grace = Some(Duration::from_millis(100));
        let (app, _, _outbox) = remote_app_with_unanswered_edits("shell-withdrawn", 1);
        let window = rt.window(app);
        rt.send(Shell::CloseRequested(window));
        tokio::time::sleep(Duration::from_millis(300)).await;
        rt.settle();
        assert_eq!(rt.asked().len(), 1);
        // Asked after the grace, of a close the OS asked for.
        assert!(
            rt.steps()
                .contains(&Step::Presented(window, Asker::Outside))
        );
        for answer in host_confirms(&rt.clew.windows[&window]) {
            rt.send(Shell::Window(window, answer));
        }
        assert!(!rt.is_up(0), "the question stands");
        assert!(!rt.open.contains(&window), "the window stayed");
        assert_eq!(rt.exits, 1);
    }

    /// While a close, or a quit, waits for the host, the window says so on
    /// its status line — it is still open and working — counting what is
    /// left as the host answers. A failure the host reports meanwhile is
    /// said, not written over.
    #[tokio::test]
    async fn a_window_says_it_waits_for_its_host() {
        let mut rt = Runtime::new();
        rt.script.borrow_mut().grace = Some(Duration::from_millis(100));
        let (app, _, _outbox) = remote_app_with_unanswered_edits("shell-waiting", 3);
        let window = rt.window(app);
        let status = |rt: &Runtime| rt.clew.windows[&window].status.clone();
        rt.send(Shell::CloseRequested(window));
        assert_eq!(
            status(&rt),
            "Closing — waiting for the host to confirm 3 changes…"
        );
        let [first, second, _] = on_the_wire(&rt, window)[..] else {
            panic!("the close sent all three");
        };
        host_answers(&mut rt, window, first, false);
        assert_eq!(
            status(&rt),
            "Closing — waiting for the host to confirm 2 changes…"
        );
        host_answers(&mut rt, window, second, true);
        assert!(
            status(&rt).starts_with(
                "Could not save .clew/bookmarks.json yet: the change to the bookmark at b.rs:2"
            ),
            "{}",
            status(&rt)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        rt.settle();
        assert_eq!(rt.asked().len(), 1);
        rt.answer(0, false);

        rt.send(Shell::Quit);
        assert_eq!(
            status(&rt),
            "Quitting — waiting for the host to confirm 1 change…"
        );
    }

    /// A quit that takes over a close keeps what its user agreed to lose:
    /// the edit agreed to is not asked about again, and once the host
    /// confirms the rest, clew quits.
    #[tokio::test]
    async fn a_quit_keeps_what_the_close_it_takes_over_was_agreed_to_lose() {
        let mut rt = Runtime::new();
        rt.script.borrow_mut().grace = Some(Duration::from_millis(100));
        let (app, _, _outbox) = remote_app_with_unanswered_edits("shell-quit-agreed", 1);
        let window = rt.window(app);
        rt.send(Shell::CloseRequested(window));
        tokio::time::sleep(Duration::from_millis(300)).await;
        rt.settle();
        // A new edit, made while the question is up, is sent once it is
        // answered; the close waits for it, and ⌘Q takes the close over.
        rt.send(remove_bookmark("b.rs", 2));
        rt.answer(0, true);
        assert!(rt.is_open(window), "closed with the new edit unconfirmed");
        rt.send(Shell::Quit);
        let [_, new] = on_the_wire(&rt, window)[..] else {
            panic!("the new edit was sent");
        };
        host_answers(&mut rt, window, new, false);
        assert_eq!(rt.asked().len(), 1, "asked again about the edit agreed to");
        assert_eq!(rt.exits, 1);
    }

    /// A close asked for while a quit waits on the hosts is not dropped:
    /// the quit closes the window with the rest, and when it is cancelled,
    /// the window closes as asked.
    #[tokio::test]
    async fn a_close_asked_for_while_a_quit_waits_is_honoured_when_it_is_cancelled() {
        let mut rt = Runtime::new();
        rt.script.borrow_mut().grace = Some(Duration::from_millis(100));
        let (app, _, _outbox) = remote_app_with_unanswered_edits("shell-held-close", 1);
        let waiting = rt.window(app);
        let idle = rt.window(blank_app());
        rt.send(Shell::Quit);
        rt.send(Shell::CloseRequested(idle));
        assert!(rt.is_open(idle), "closed before the quit's outcome");
        tokio::time::sleep(Duration::from_millis(300)).await;
        rt.settle();
        assert_eq!(rt.asked().len(), 1);
        rt.answer(0, false);
        assert!(!rt.open.contains(&idle), "the close asked for was dropped");
        assert!(rt.is_open(waiting));
        assert_eq!(rt.exits, 0);
    }

    /// Every window's sheets are ended before it closes, whether or not
    /// clew asked anything on it: a file picker is a sheet on the window it
    /// was opened from, and a window closed under one left AppKit a sheet
    /// on a window that is gone.
    #[tokio::test]
    async fn a_window_with_nothing_to_ask_still_has_its_sheets_ended_as_it_closes() {
        let mut rt = Runtime::new();
        let window = rt.window(blank_app());
        rt.send(Shell::Window(window, Message::Window(WindowMsg::Close)));
        let steps = rt.steps();
        assert_eq!(steps, [Step::Dismissed(window), Step::Closed(window)]);
    }

    /// The file pickers — Open Folder, the Connect form's SSH key — are
    /// begun from the callback of the window they were opened from, as
    /// sheets on it: it is their parent, whose close ends them.
    #[tokio::test]
    async fn the_file_pickers_are_sheets_on_their_window() {
        let mut rt = Runtime::on(Box::new(Native), Rc::default());
        let window = rt.window(blank_app());
        rt.send(Shell::Window(
            window,
            Message::Project(crate::ProjectMsg::OpenFolderPressed),
        ));
        rt.send(Shell::Window(
            window,
            Message::Connect(crate::ConnectMsg::PickIdentity),
        ));
        assert_eq!(rt.callbacks, [window, window]);
    }

    fn running_session() -> DebugSession {
        DebugSession {
            client: None,
            status: DebugStatus::Running,
            thread_id: None,
            frames: Vec::new(),
            scopes: Vec::new(),
            watches: Vec::new(),
            output: Vec::new(),
            current: None,
            program: PathBuf::from("/tmp/prog"),
            args: Vec::new(),
            cwd: PathBuf::from("/tmp"),
            addr: None,
        }
    }

    /// Closing a window used to drop its `App` and nothing else, which left the
    /// debug run with no owner: the startup stream that holds the adapter
    /// process runs on the daemon's runtime, and the run counter it polls —
    /// the ONLY way to cancel it — was never moved, so the adapter and the
    /// debuggee ran on with no window left to stop them. The teardown must
    /// also stay window-scoped: another window's session is not this one's to
    /// end.
    #[test]
    fn closing_a_window_ends_its_debug_run_and_leaves_the_other_windows_alone() {
        let mut clew = test_clew();
        let (closing, staying) = (window::Id::unique(), window::Id::unique());
        for id in [closing, staying] {
            let mut app = blank_app();
            app.main_window = Some(id);
            app.debug.session = Some(running_session());
            app.bump_debug_run(); // as start_debug does: this run's identity
            clew.windows.insert(id, app);
        }
        clew.focused = Some(closing);
        // The live counters outlive their `App`s exactly as the startup
        // streams' clones do — that is what makes the bump observable here.
        let closing_live = clew.windows[&closing].debug_run_live.clone();
        let staying_live = clew.windows[&staying].debug_run_live.clone();
        let (before_closing, before_staying) = (
            closing_live.load(Ordering::SeqCst),
            staying_live.load(Ordering::SeqCst),
        );

        let _ = update(&mut clew, Shell::CloseRequested(closing));

        assert_ne!(
            closing_live.load(Ordering::SeqCst),
            before_closing,
            "the closed window's startup stream was never told to cancel"
        );
        assert_eq!(
            staying_live.load(Ordering::SeqCst),
            before_staying,
            "closing one window must not cancel another window's debug run"
        );
        assert!(
            clew.windows[&staying].debug.session.is_some(),
            "the surviving window's session was torn down with its neighbour"
        );
        assert!(!clew.windows.contains_key(&closing));
        assert_eq!(clew.focused, Some(staying));

        // A window that closed without asking (the OS closed it) is torn
        // down all the same, once it is gone.
        let _ = update(&mut clew, Shell::Closed(staying));
        assert_ne!(
            staying_live.load(Ordering::SeqCst),
            before_staying,
            "a window closed unasked kept its debug run"
        );
        assert!(clew.windows.is_empty());
    }

    /// ⌘Q reached AppKit's `terminate:`, which exits the process from inside
    /// `-[NSApplication run]`. No window was ever asked to close, so no window
    /// emitted `Closed`, so the teardown above ran for *nobody* — every debug
    /// adapter and every debuggee they launched was abandoned, in every window,
    /// on the way users most often quit. A quit has to be that same teardown,
    /// once per window.
    #[test]
    fn quitting_ends_every_windows_debug_run() {
        let mut clew = test_clew();
        let ids = [window::Id::unique(), window::Id::unique()];
        for id in ids {
            let mut app = blank_app();
            app.main_window = Some(id);
            app.debug.session = Some(running_session());
            app.bump_debug_run();
            clew.windows.insert(id, app);
        }
        clew.focused = Some(ids[0]);
        // As in the close test: the counters are what the startup streams hold,
        // so they are what makes the cancellation observable.
        let live: Vec<_> = ids
            .iter()
            .map(|id| clew.windows[id].debug_run_live.clone())
            .collect();
        let before: Vec<u64> = live.iter().map(|l| l.load(Ordering::SeqCst)).collect();

        for app in clew.windows.values_mut() {
            app.pending_scan_root = Some(std::path::PathBuf::from("/p"));
            assert!(app.wants_server());
        }

        let _ = update(&mut clew, Shell::Quit);

        // Each window is gone, and its subscription with it: its clew-server
        // sees EOF and gets its grace (which the exit waits for), and none
        // is restarted.
        assert!(clew.windows.is_empty(), "a quitting window was kept");
        for (i, counter) in live.iter().enumerate() {
            assert_ne!(
                counter.load(Ordering::SeqCst),
                before[i],
                "window {i}'s startup stream was never told to cancel, so its \
                 adapter and debuggee outlive the app"
            );
        }
    }

    /// A window can close just as the update download it started finishes:
    /// closing aborts the download, but its result is already on its way, so
    /// the finished DMG arrives for a window that no longer exists and the
    /// message is dropped. Dropping the message used to mean stranding a whole
    /// release image in a per-attempt directory that nothing else ever visits —
    /// not the install path, not the next launch, and (since the destination
    /// moved off the temp dir) not a reboot either.
    #[test]
    fn a_download_that_outlives_its_window_does_not_outlive_the_process() {
        let mut clew = test_clew();
        let root = test_dir("orphan-dl");
        let dir = crate::updater::create_private_dir(&root, crate::updater::DOWNLOAD_PREFIX)
            .expect("a download directory");
        let dmg = dir.join("Clew-9.9.9.dmg");
        std::fs::write(&dmg, b"image").unwrap();

        let _ = update(
            &mut clew,
            Shell::Window(
                window::Id::unique(),
                Message::Updater(UpdaterMsg::Downloaded {
                    generation: 0,
                    result: Ok(dmg),
                }),
            ),
        );

        assert!(
            !dir.exists(),
            "the bytes of a download nobody can install any more were left behind"
        );
    }

    /// A window that closes mid-download takes the download with it. iced
    /// keeps draining a closed window's streams, so the transfer used to run
    /// to its end — up to two hours on a slow link — only for the image to be
    /// thrown away on arrival. Closing aborts it: the transfer stops, the
    /// attempt's directory goes, and the run delivers nothing. The abort is
    /// the closing window's alone: another window's download runs on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closing_a_window_stops_its_update_download_and_no_other() {
        use crate::UpdatePhase;
        use crate::app::tests::run_task_async;
        use crate::updater::testing::{download_dirs, eventually, stalling_host};
        use std::time::Duration;
        let root = test_dir("close-dl");
        let _env = data_dir_override(&root);
        let (url, served) = stalling_host(2);
        let version = clew_core::update::Version::parse("9.9.9").unwrap();
        let mut clew = test_clew();
        let (closing, staying) = (window::Id::unique(), window::Id::unique());
        let mut runs = HashMap::new();
        let mut dirs: HashMap<window::Id, PathBuf> = HashMap::new();
        // One at a time, so each window's attempt directory is known.
        for id in [closing, staying] {
            let mut app = blank_app();
            app.main_window = Some(id);
            let task = app.start_update_download(url.clone(), version);
            clew.windows.insert(id, app);
            runs.insert(id, tokio::spawn(run_task_async(task)));
            served
                .recv_timeout(Duration::from_secs(10))
                .expect("the download never started");
            let new: Vec<PathBuf> = download_dirs(&root)
                .into_iter()
                .filter(|d| !dirs.values().any(|known| known == d))
                .collect();
            assert_eq!(new.len(), 1, "{new:?}");
            dirs.insert(id, new[0].clone());
        }
        clew.focused = Some(closing);

        let _ = update(&mut clew, Shell::Closed(closing));

        eventually("the closed window's download kept running", || {
            !dirs[&closing].exists()
        })
        .await;
        let closed_run = runs.remove(&closing).unwrap();
        let delivered = tokio::time::timeout(Duration::from_secs(5), closed_run)
            .await
            .expect("the closed window's download task kept running")
            .unwrap();
        assert!(
            !delivered
                .iter()
                .any(|m| matches!(m, Message::Updater(UpdaterMsg::Downloaded { .. }))),
            "{delivered:?}"
        );
        // The other window's download was not touched.
        let other = clew.windows.get_mut(&staying).unwrap();
        assert_eq!(other.update.phase, UpdatePhase::Downloading);
        assert!(
            other
                .update
                .download
                .as_ref()
                .is_some_and(|d| !d.is_aborted()),
            "closing one window aborted another's download"
        );
        assert!(dirs[&staying].exists());
        assert!(!runs[&staying].is_finished());

        // Stop it too, so nothing outlives the test.
        other.abort_update_download();
        let _ = tokio::time::timeout(Duration::from_secs(5), runs.remove(&staying).unwrap()).await;
        eventually("the other window's download never stopped", || {
            !dirs[&staying].exists()
        })
        .await;
    }

    fn key(c: &str) -> keyboard::Key {
        keyboard::Key::Character(c.into())
    }

    /// The finding: `listen_with` forwarded every KeyPressed, including the
    /// ones a focused text field had already consumed, so typing "git" into
    /// the search box ran `g` `i` = Go to Implementation behind it, and `j`,
    /// `za`, `G` moved and folded the code view.
    #[test]
    fn keys_a_text_field_consumed_do_not_reach_the_window() {
        use iced::event::Status::{Captured, Ignored};
        use keyboard::Modifiers;
        let none = Modifiers::empty();
        for c in ["j", "g", "i", "z", "a", "G", "0", "$"] {
            assert!(!forward_key(&key(c), none, Captured), "typed {c:?}");
            assert!(forward_key(&key(c), none, Ignored), "unfocused {c:?}");
        }
        // Esc closes overlays even from inside their own input.
        let esc = keyboard::Key::Named(keyboard::key::Named::Escape);
        assert!(forward_key(&esc, none, Captured));
        // ⌘-shortcuts keep working from inside a field…
        for c in ["p", "t", "f", "l", "d", "="] {
            assert!(forward_key(&key(c), Modifiers::COMMAND, Captured), "⌘{c}");
        }
        // …but not the field's own editing and motion chords.
        for c in ["c", "x", "v", "a", "z"] {
            assert!(!forward_key(&key(c), Modifiers::COMMAND, Captured), "⌘{c}");
        }
        let left = keyboard::Key::Named(keyboard::key::Named::ArrowLeft);
        assert!(
            !forward_key(&left, Modifiers::ALT, Captured),
            "⌥← is word motion in the field, not Back"
        );
        assert!(!forward_key(&left, Modifiers::COMMAND, Captured));
        assert!(
            forward_key(&left, Modifiers::ALT, Ignored),
            "with no field focused ⌥← is still Back"
        );
        assert!(!forward_key(&key("p"), Modifiers::CTRL, Captured));
    }

    /// The router applies that rule to real events and leaves the rest alone.
    #[test]
    fn the_router_drops_only_consumed_key_presses() {
        let id = window::Id::unique();
        let press = |c: &str| {
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: key(c),
                modified_key: key(c),
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: Some(c.into()),
                repeat: false,
            })
        };
        assert!(route(press("j"), iced::event::Status::Captured, id).is_none());
        assert!(matches!(
            route(press("j"), iced::event::Status::Ignored, id),
            Some(Shell::Window(w, Message::Editor(EditorMsg::KeyPressed(..)))) if w == id
        ));
        // Window lifecycle is routed whatever the capture status.
        assert!(matches!(
            route(
                iced::Event::Window(iced::window::Event::Closed),
                iced::event::Status::Captured,
                id
            ),
            Some(Shell::Closed(w)) if w == id
        ));
        // A close the OS asks for is a request, which the shell decides on.
        assert!(matches!(
            route(
                iced::Event::Window(iced::window::Event::CloseRequested),
                iced::event::Status::Ignored,
                id
            ),
            Some(Shell::CloseRequested(w)) if w == id
        ));
        assert!(
            !crate::window_settings().exit_on_close_request,
            "iced would close the window itself, unasked"
        );
    }

    /// A rebind in one window used to change that window alone: the others
    /// kept dispatching the old chord until reopened. The shell now hands the
    /// new bindings to every window.
    #[test]
    fn a_rebind_in_one_window_reaches_every_window() {
        use crate::keymap::{Action, Chord};
        // The capture persists the binding to config.toml: point that at a
        // private directory, never the user's.
        let data = test_dir("shell-keymap");
        std::fs::create_dir_all(&data).unwrap();
        let _env = data_dir_override(&data);
        let mut clew = test_clew();
        let (acting, other) = (window::Id::unique(), window::Id::unique());
        for id in [acting, other] {
            let mut app = blank_app();
            app.main_window = Some(id);
            clew.windows.insert(id, app);
        }
        // What a successful capture does to the acting window's copy; driven
        // through a real message so the shell sees it across an update.
        clew.windows.get_mut(&acting).unwrap().rebinding = Some(Action::GotoLine);
        let _ = update(
            &mut clew,
            Shell::Window(
                acting,
                Message::Editor(EditorMsg::KeyPressed(
                    key("g"),
                    keyboard::Modifiers::COMMAND,
                )),
            ),
        );
        let acting_chord = clew.windows[&acting].keymap.chord(Action::GotoLine);
        assert_eq!(acting_chord, Chord::cmd('g'), "the capture itself happened");
        assert_eq!(
            clew.windows[&other].keymap.chord(Action::GotoLine),
            Chord::cmd('g'),
            "the other window still fires the old chord"
        );
        assert_eq!(
            clew.windows[&other].keymap.action_for(&Chord::cmd('l')),
            None,
            "the old chord must stop firing everywhere"
        );
    }

    /// An installed update quits the app, and that quit is ⌘Q's: every
    /// window's teardown. The installing window used to exit on its own and
    /// leave the other windows' debug adapters and debuggees running.
    #[test]
    fn an_installed_update_quits_through_every_windows_teardown() {
        let mut clew = test_clew();
        let ids = [window::Id::unique(), window::Id::unique()];
        for id in ids {
            let mut app = blank_app();
            app.main_window = Some(id);
            app.debug.session = Some(running_session());
            app.bump_debug_run();
            clew.windows.insert(id, app);
        }
        let live: Vec<_> = ids
            .iter()
            .map(|id| clew.windows[id].debug_run_live.clone())
            .collect();
        let before: Vec<u64> = live.iter().map(|l| l.load(Ordering::SeqCst)).collect();

        let _ = update(
            &mut clew,
            Shell::Window(ids[0], Message::Updater(UpdaterMsg::Installed(Ok(())))),
        );

        for (i, counter) in live.iter().enumerate() {
            assert_ne!(
                counter.load(Ordering::SeqCst),
                before[i],
                "window {i}'s debug run survived the update's quit"
            );
        }
    }
}
