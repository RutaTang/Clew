//! The multi-window shell.
//!
//! clew runs under an iced daemon: this layer holds one independent [`App`] per
//! window and routes each window's messages back to *its own* `App`. The trick
//! is `map`: a window's view maps its `Message`s to `Shell::Window(id, _)`, so a
//! click and the async Tasks it spawns always return to the window that produced
//! them. Global input events carry the originating window id from `listen_with`;
//! the app-global menu bar targets the focused window via `Shell::ToFocused`.

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
}

/// Open the first window and seed its `App`.
pub fn boot() -> (Clew, Task<Shell>) {
    let mut clew = Clew { windows: HashMap::new(), focused: None };
    // The first window restores the last-opened project; New Window opens empty.
    let task = clew.open_window(true);
    (clew, task)
}

impl Clew {
    /// Open a new window with a fresh `App`. `restore` reopens the last project
    /// (used for the first window at launch); otherwise the window starts empty,
    /// ready for the user to open a folder — the standard "New Window".
    fn open_window(&mut self, restore: bool) -> Task<Shell> {
        let (mut app, init) =
            if restore { App::new() } else { (App::blank(), Task::none()) };
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
            match clew.windows.get_mut(&id) {
                Some(app) => app.update(msg).map(move |m| Shell::Window(id, m)),
                None => Task::none(),
            }
        }
        Shell::ToFocused(msg) => match clew.focused {
            Some(id) => update(clew, Shell::Window(id, msg)),
            None => Task::none(),
        },
        Shell::NewWindow => clew.open_window(false),
        Shell::Opened(_id) => {
            // Frameless windows lose the OS rounded corners; restore them, and
            // install the native menu bar (both main-thread; the menu once).
            #[cfg(target_os = "macos")]
            {
                crate::macos::round_corners(10.0);
                crate::macos::menu::install_once();
            }
            Task::none()
        }
        Shell::Closed(id) => {
            clew.windows.remove(&id);
            if clew.focused == Some(id) {
                clew.focused = clew.windows.keys().next().copied();
            }
            if clew.windows.is_empty() {
                iced::exit()
            } else {
                Task::none()
            }
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
    clew.windows.get(&id).map(App::title).unwrap_or_else(|| "clew".to_string())
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
        subs.push(app.window_subscription().with(id).map(|(id, m)| Shell::Window(id, m)));
    }
    // The app-global menu bar (one, at the top of the screen): app commands go
    // to the focused window; New Window is handled by the shell.
    #[cfg(target_os = "macos")]
    subs.push(crate::macos::menu::subscription().map(shell_from_menu));

    Subscription::batch(subs)
}

/// Translate an app-global menu command into a shell message.
#[cfg(target_os = "macos")]
fn shell_from_menu(cmd: crate::macos::menu::MenuCmd) -> Shell {
    use crate::macos::menu::MenuCmd;
    match cmd {
        MenuCmd::App(m) => Shell::ToFocused(m),
        MenuCmd::NewWindow => Shell::NewWindow,
    }
}
