//! clew — a code reader.
//!
//! v2: gitignore-aware file tree, virtualized tree-sitter code viewer,
//! fuzzy file finder (Cmd+P), project symbol search (Cmd+T), full-text
//! search (Cmd+Shift+F), navigation history, outline, split view (Cmd+\),
//! line selection + copy (Cmd+C), bookmarks (Cmd+D), go-to-line (:N).
// A release build on Windows is a GUI program: started from Explorer or the
// Start menu it opens its window and no console beside it. (Debug builds keep
// the console, for what they log.) Every child it starts is started without a
// console window of its own: `clew_core::spawn`.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod dap;
pub use clew_core::docs;
pub use clew_core::explain;
// Moved into the shared `clew-core` crate (used by both the GUI and the headless
// server); re-exported so existing `crate::fs_scan` / `crate::search` paths hold.
pub use clew_core::fs_scan;
pub use clew_core::git;
pub use clew_core::inactive;
pub use clew_core::incremental;
pub use clew_core::llm;
pub use clew_core::lsp;
#[cfg(target_os = "macos")]
mod macos;
pub use clew_core::outline;
pub use clew_core::search;
mod ui;
pub use clew_core::embed;

// `App`'s update / message-handling methods live here, split by area. The model
// (`App` + its state sub-structs) and the `Message` enum live in `app::state` /
// `app::message`; they are re-exported here so `crate::App` / `crate::Message`
// paths hold across the codebase.

// Feature modules, grouped into folders for navigation. Each group re-exports
// its modules at the crate root so existing `crate::<module>` paths hold.
mod editor;
pub(crate) use editor::{analyze, codeview, find, flow, highlight, viewer};
mod graph;
pub(crate) use graph::{
    callgraph, graphlayout, imports, index, projectcalls, structure, typegraph,
};
mod ai;
pub(crate) use ai::{fakerun, overview, render, richmd, walkthrough};
mod session;
pub(crate) use session::{bookmarks, cache, finder, history, notes, reading};
mod backend;
pub(crate) use backend::{connect, langenv, server, sources, updater, watch};
mod miscellaneous;
pub(crate) use miscellaneous::{fonts, glyph, i18n, icons, keymap, resize, stats, theme};

mod app;
mod shell;
pub use app::message::{
    AskMsg, CallsMsg, ConnectMsg, ContentMsg, DebugMsg, DocsMsg, EditorMsg, ExplainMsg, ExportMsg,
    FakeMsg, FlowMsg, GlossaryMsg, GraphMsg, HistoryMsg, HoverMsg, IntegrationsMsg, LangField,
    LanguagesMsg, LspMsg, Message, NavMsg, OverviewMsg, ProjectMsg, ProofMsg, ReadingMsg,
    SemanticMsg, ServerMsg, SettingsMsg, SimCmd, TimeTravelMsg, TutorialMsg, UpdaterMsg, WalkMsg,
    WatchMsg, WeakMsg, WindowMsg,
};
pub(crate) use app::message::{Handoff, Origin, Stamp};
pub(crate) use app::model::*;
pub(crate) use app::rpc::*;
pub use app::state::{
    App, AvailableUpdate, CheckOutcome, DEFAULT_FONT_SIZE, DebugState, DocsState, DocsView,
    EditIds, ExplainState, ImportWork, InFlight, LostEdits, OverviewState, PaneOpen, PendingAnchor,
    PendingAsk, ProjectCallsState, ProjectLink, ProjectSession, RemoteEdit, SettingsDraft,
    SettingsPage, StatsState, SupersededOpen, UpdatePhase, UpdateState, WalkState, WalkUi,
};
pub(crate) use app::tasks::*;
pub use app::voice::VoiceMsg;

use iced::Size;

use crate::bookmarks::Bookmark;
use crate::fs_scan::FileEntry;
use crate::viewer::MAX_FILE_BYTES;

pub fn main() -> iced::Result {
    // MCP servers are told this app's version, not its library's.
    clew_core::mcp::set_client_version(env!("CARGO_PKG_VERSION"));
    // Which window system the windows will be on, read before iced connects
    // to it: Wayland's client takes `WAYLAND_SOCKET` out of the environment
    // as it does.
    app::note_window_system();
    // On Windows, whether the windows present through a visual, for their
    // corners to be the design's: decided before iced makes the renderer,
    // which reads it, and the first window, made to suit it.
    #[cfg(windows)]
    app::corners::decide_presentation();
    // The user's login-shell PATH (asked of the shell once, for an app opened
    // from the Finder) is found now, off the UI thread: the first MCP server
    // or ACP agent started, or added in the settings, would wait for it.
    std::thread::spawn(|| {
        let _ = clew_core::linerpc::spawn_path();
    });
    // A multi-window daemon: the `shell` layer holds one App per window and
    // routes each window's messages to its own App (see `crate::shell`).
    let app = iced::daemon(shell::boot, shell::update, shell::view)
        .title(shell::title)
        .theme(shell::theme)
        .subscription(shell::subscription)
        .style(shell::style)
        // Embed the icon font (Nerd Font symbols) for file-type icons, and
        // the two typefaces Clew is set in: Inter, the interface's default,
        // and JetBrains Mono for code.
        .font(icons::FONT_BYTES)
        .default_font(fonts::UI);
    fonts::sources().fold(app, |app, face| app.font(face)).run()
}

/// Frameless-looking window settings.
///
/// clew draws its own window controls (the red/amber/green buttons) in its
/// toolbar, so it wants no visible OS title bar. It gets that look while staying
/// a *standard* titled window, which matters because tiling window managers
/// (AeroSpace, yabai) drive windows through the Accessibility API and only
/// manage windows that report the `AXStandardWindow` subrole — a plain
/// borderless window does not, so it silently drops out of tiling.
///
/// So on macOS we keep `decorations` on (a real titled window) but hide the
/// title, make the title bar transparent, and let the content fill the whole
/// frame. `macos::configure_frameless` then hides the native traffic lights and
/// the title-bar view itself, so the top strip behaves exactly like a frameless
/// window: clew's own controls capture their clicks and dragging empty toolbar
/// space moves the window. On other platforms there is no such trick, so the
/// window stays borderless.
pub(crate) fn window_settings() -> iced::window::Settings {
    window_settings_for(app::corners::through_visual())
}

/// [`window_settings`] for a window that presents through a visual
/// (`through_visual`, only ever on Windows; see `app::corners`) or not.
pub(crate) fn window_settings_for(through_visual: bool) -> iced::window::Settings {
    #[cfg(target_os = "macos")]
    let platform_specific = iced::window::settings::PlatformSpecific {
        // A titled window that looks frameless: no title text, a transparent
        // title bar, and content drawn all the way to the top edge.
        title_hidden: true,
        titlebar_transparent: true,
        fullsize_content_view: true,
    };
    // A window presented through a visual has the design's corners, left
    // clear by Clew's own drawing, and Windows is asked to round none of
    // its own; otherwise Windows 11 rounds the borderless window's corners
    // itself, to its own radius (Windows 10 has none to round with). See
    // `app::corners`.
    #[cfg(target_os = "windows")]
    let platform_specific = {
        use iced::window::settings::platform::CornerPreference;
        iced::window::settings::PlatformSpecific {
            corner_preference: if through_visual {
                CornerPreference::DoNotRound
            } else {
                CornerPreference::Round
            },
            ..iced::window::settings::PlatformSpecific::default()
        }
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let platform_specific = iced::window::settings::PlatformSpecific::default();

    iced::window::Settings {
        size: Size::new(1280.0, 860.0),
        // Below this the chrome no longer fits: the toolbar's two clusters
        // collide and the modals lose their margins.
        min_size: Some(Size::new(760.0, 520.0)),
        position: iced::window::Position::Centered,
        // macOS keeps decorations on (a standard, tile-manageable window) and
        // hides the chrome in AppKit; elsewhere, borderless is the frameless look.
        decorations: cfg!(target_os = "macos"),
        // The rounded corners are clipped from the content layer on macOS (see
        // `macos::configure_frameless`), and left clear by Clew's own drawing
        // on Linux and Windows (`ui::window_radius_for`): either needs the
        // window surface to carry an alpha channel, for the corners to
        // composite over the desktop. On Windows only a window presented
        // through a visual is composited by its alpha (`app::corners`), and
        // any other is made opaque: a transparent one is made without the
        // surface GDI draws to, and shows only what is presented so.
        transparent: !cfg!(target_os = "windows") || through_visual,
        // A close the OS asks for (a window manager, the Accessibility API)
        // comes to the shell as a request, like ⌘W: a window holding edits it
        // cannot send asks before it goes (`Shell::CloseRequested`). Left on,
        // iced closed the window itself and nothing was asked.
        exit_on_close_request: false,
        platform_specific,
        ..iced::window::Settings::default()
    }
}
