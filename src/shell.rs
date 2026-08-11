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
//! Every exit clew itself performs goes through [`exit_after`], because the work
//! a window owns is not all held by its `App` — a debug adapter and its debuggee
//! belong to a stream on the daemon's runtime, and the only thing that stops
//! them is a teardown that runs *before* the process goes away. What that CANNOT
//! cover is a process death clew is not asked about: the Dock's Quit item, Force
//! Quit, a log-out, or a crash all reach `terminate:`/`SIGKILL` without any
//! message arriving here, and there the adapter is still on its own.

use std::collections::HashMap;

use iced::{Element, Subscription, Task, Theme, keyboard, window};

use crate::{App, Message};

/// The daemon's top-level state: one `App` per open window.
pub struct Clew {
    windows: HashMap<window::Id, App>,
    /// The window that most recently gained focus — the target of the app-global
    /// menu bar and any "current window" action.
    focused: Option<window::Id>,
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
    /// A window closed; drop its `App`, and exit once the last one goes.
    Closed(window::Id),
    /// Quit the app: tear every window's work down, then exit.
    Quit,
}

/// How long a quit waits for the windows' teardown before exiting regardless.
///
/// The teardown's own bound is the DAP client's 60 s request timeout, which is a
/// reasonable wait for a live adapter and a terrible one for ⌘Q: an adapter that
/// has stopped answering would hold the whole app open. Long enough for a
/// `disconnect` round trip on a working adapter, short enough that a wedged one
/// is not felt.
const TEARDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Exit the runtime once `teardown` has finished, or after [`TEARDOWN_GRACE`],
/// whichever comes first.
///
/// `Task::batch` is a `SelectAll`, so batching the teardown with `iced::exit()`
/// — which is what the last-window path did — sequenced nothing: `exit` is an
/// immediately-ready effect while the teardown needs a request and a response,
/// so Exit always won and the adapter was abandoned by every quit. `chain` is
/// what actually waits. The second arm is the counterweight the first one needs:
/// nothing may make quitting depend on an adapter answering.
pub(crate) fn exit_after<T: Send + 'static>(teardown: Task<T>) -> Task<T> {
    Task::batch([
        teardown.chain(iced::exit()),
        // Built inside the future so no timer is armed (and no runtime is
        // required) until this actually runs.
        Task::future(async { tokio::time::sleep(TEARDOWN_GRACE).await })
            .discard()
            .chain(iced::exit()),
    ])
}

/// Open the first window and seed its `App`.
pub fn boot() -> (Clew, Task<Shell>) {
    let mut clew = Clew {
        windows: HashMap::new(),
        focused: None,
    };
    // Downloads abandoned by a clew that died mid-stream are nobody's to clean
    // up at runtime, and the data root they now live in is never swept by the
    // OS. Off-thread: it can be removing a disk image, and nothing waits on it.
    std::thread::spawn(crate::updater::sweep_stale_downloads);
    // The first window restores the last-opened project; New Window opens empty.
    let task = clew.open_window(true);
    (clew, task)
}

impl Clew {
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
}

pub fn update(clew: &mut Clew, message: Shell) -> Task<Shell> {
    match message {
        Shell::Window(id, msg) => {
            // Track focus so the menu / global actions hit the right window.
            if matches!(msg, Message::WindowFocusChanged(true)) {
                clew.focused = Some(id);
            }
            // The appearance palette is process-global while `App`s are not,
            // so one window changing it repaints all of them — but only the
            // acting window learns of it. The others kept a stale preference
            // and, worse, kept their cached diagram SVGs in the previous
            // colors indefinitely. Watch the global revision across this
            // window's update and tell the rest to catch up.
            let before = crate::theme::revision();
            let task = match clew.windows.get_mut(&id) {
                Some(app) => app.update(msg).map(move |m| Shell::Window(id, m)),
                // The window is gone; its async work is not. iced keeps
                // draining the streams it started, and an update download that
                // finishes now has nobody left to install it — a whole release
                // image, in a per-attempt directory nothing else ever visits.
                // Dropping the message has to mean dropping the bytes too.
                None => {
                    if let Message::UpdateDownloaded {
                        result: Ok(dmg), ..
                    } = &msg
                    {
                        crate::updater::discard_download(dmg);
                    }
                    Task::none()
                }
            };
            if crate::theme::revision() == before {
                return task;
            }
            let resync: Vec<Task<Shell>> = clew
                .windows
                .keys()
                .copied()
                .filter(|other| *other != id)
                .map(|other| Task::done(Shell::Window(other, Message::ThemeResynced)))
                .collect();
            Task::batch([task, Task::batch(resync)])
        }
        Shell::ToFocused(msg) => match clew.focused {
            Some(id) => update(clew, Shell::Window(id, msg)),
            None => Task::none(),
        },
        Shell::NewWindow => clew.open_window(false),
        Shell::Opened(_id) => {
            // Strip the OS chrome to the frameless look and round the corners,
            // and install the native menu bar (both main-thread; the menu once).
            #[cfg(target_os = "macos")]
            {
                crate::macos::configure_frameless(10.0);
                crate::macos::menu::install_once();
                crate::macos::appearance::install_once();
            }
            Task::none()
        }
        Shell::Closed(id) => {
            // Dropping the `App` is NOT enough to end the work this window
            // started. Its debug adapter — and the debuggee that adapter
            // launched — are processes owned by the startup stream, which
            // iced keeps draining from the daemon's runtime after the window
            // is gone; the stream's only cancellation seam is the run counter
            // `on_window_closed` bumps. Without this a locally spawned
            // adapter (dlv, js-debug, or any adapter when no clew-server
            // resolved) kept running with no window left to stop it.
            let teardown = match clew.windows.get_mut(&id) {
                Some(app) => app.on_window_closed().map(move |m| Shell::Window(id, m)),
                None => Task::none(),
            };
            clew.windows.remove(&id);
            if clew.focused == Some(id) {
                clew.focused = clew.windows.keys().next().copied();
            }
            if clew.windows.is_empty() {
                exit_after(teardown)
            } else {
                teardown
            }
        }
        Shell::Quit => {
            // ⌘Q used to reach AppKit's `terminate:`, which calls `exit(0)`
            // from inside `-[NSApplication run]`: no window ever emitted
            // `Closed`, so no window's teardown ran, and no Rust destructor ran
            // either — the debug adapter and the program it launched were
            // simply abandoned. Quit is now the same teardown ⌘W does, once per
            // window, with the exit sequenced after all of them.
            //
            // The teardown runs here rather than by closing each window and
            // waiting for its `Closed`: an exit that only happens once the
            // window map empties is an exit that never happens if one close is
            // dropped, and a quit that can hang is worse than the leak it was
            // meant to fix. The `App`s stay in the map so the windows keep
            // rendering for the (usually zero) time the teardown takes.
            let ids: Vec<window::Id> = clew.windows.keys().copied().collect();
            let teardown: Vec<Task<Shell>> = ids
                .into_iter()
                .filter_map(|id| {
                    let app = clew.windows.get_mut(&id)?;
                    Some(app.on_window_closed().map(move |m| Shell::Window(id, m)))
                })
                .collect();
            exit_after(Task::batch(teardown))
        }
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

pub fn subscription(clew: &Clew) -> Subscription<Shell> {
    // Global input events (listen_with sees events already captured by focused
    // widgets, so Esc works while a text input has focus), routed to the window
    // they occurred in.
    let events = iced::event::listen_with(|event, _status, window| match event {
        iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            Some(Shell::Window(window, Message::KeyPressed(key, modifiers)))
        }
        iced::Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => {
            Some(Shell::Window(window, Message::ModifiersChanged(m)))
        }
        iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) => {
            Some(Shell::Window(window, Message::SelectEnd))
        }
        iced::Event::Window(iced::window::Event::Resized(size)) => {
            Some(Shell::Window(window, Message::WindowResized(size)))
        }
        iced::Event::Window(iced::window::Event::Opened { .. }) => Some(Shell::Opened(window)),
        // A close request (⌘W / the red control / the OS) only *asks*; route it
        // to that window's App, which actually closes its own window.
        iced::Event::Window(iced::window::Event::CloseRequested) => {
            Some(Shell::Window(window, Message::CloseWindow))
        }
        iced::Event::Window(iced::window::Event::Closed) => Some(Shell::Closed(window)),
        iced::Event::Window(iced::window::Event::Focused) => {
            Some(Shell::Window(window, Message::WindowFocusChanged(true)))
        }
        iced::Event::Window(iced::window::Event::Unfocused) => {
            Some(Shell::Window(window, Message::WindowFocusChanged(false)))
        }
        _ => None,
    });

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
    use crate::{DebugSession, DebugStatus};
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

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
            port: None,
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
        let mut clew = Clew {
            windows: HashMap::new(),
            focused: None,
        };
        let (closing, staying) = (window::Id::unique(), window::Id::unique());
        for id in [closing, staying] {
            let mut app = App::blank();
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

        let _ = update(&mut clew, Shell::Closed(closing));

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
    }

    /// ⌘Q reached AppKit's `terminate:`, which exits the process from inside
    /// `-[NSApplication run]`. No window was ever asked to close, so no window
    /// emitted `Closed`, so the teardown above ran for *nobody* — every debug
    /// adapter and every debuggee they launched was abandoned, in every window,
    /// on the way users most often quit. A quit has to be that same teardown,
    /// once per window.
    #[test]
    fn quitting_ends_every_windows_debug_run() {
        let mut clew = Clew {
            windows: HashMap::new(),
            focused: None,
        };
        let ids = [window::Id::unique(), window::Id::unique()];
        for id in ids {
            let mut app = App::blank();
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

        let _ = update(&mut clew, Shell::Quit);

        for (i, counter) in live.iter().enumerate() {
            assert_ne!(
                counter.load(Ordering::SeqCst),
                before[i],
                "window {i}'s startup stream was never told to cancel, so its \
                 adapter and debuggee outlive the app"
            );
        }
    }

    /// A window can close while the update download it started is still
    /// streaming: iced keeps draining that stream from the daemon's runtime, so
    /// the finished DMG arrives for a window that no longer exists and the
    /// message is dropped. Dropping the message used to mean stranding a whole
    /// release image in a per-attempt directory that nothing else ever visits —
    /// not the install path, not the next launch, and (since the destination
    /// moved off the temp dir) not a reboot either.
    #[test]
    fn a_download_that_outlives_its_window_does_not_outlive_the_process() {
        let mut clew = Clew {
            windows: HashMap::new(),
            focused: None,
        };
        let root = std::env::temp_dir().join(format!("clew-orphan-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = crate::updater::create_private_dir(&root, crate::updater::DOWNLOAD_PREFIX)
            .expect("a download directory");
        let dmg = dir.join("Clew-9.9.9.dmg");
        std::fs::write(&dmg, b"image").unwrap();

        let _ = update(
            &mut clew,
            Shell::Window(
                window::Id::unique(),
                Message::UpdateDownloaded {
                    generation: 0,
                    result: Ok(dmg),
                },
            ),
        );

        assert!(
            !dir.exists(),
            "the bytes of a download nobody can install any more were left behind"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
