//! clew — a code reader.
//!
//! v2: gitignore-aware file tree, virtualized tree-sitter code viewer,
//! fuzzy file finder (Cmd+P), project symbol search (Cmd+T), full-text
//! search (Cmd+Shift+F), navigation history, outline, split view (Cmd+\),
//! line selection + copy (Cmd+C), bookmarks (Cmd+D), go-to-line (:N).

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
pub(crate) use editor::{analyze, codeview, find, highlight, viewer};
mod graph;
pub(crate) use graph::{callgraph, graphlayout, imports, index, projectcalls, structure};
mod ai;
pub(crate) use ai::{overview, render, richmd, walkthrough};
mod session;
pub(crate) use session::{bookmarks, cache, finder, history, notes, reading};
mod backend;
pub(crate) use backend::{connect, langenv, server, watch};
mod miscellaneous;
pub(crate) use miscellaneous::{glyph, icons, keymap, resize, stats, theme};

mod app;
mod shell;
pub use app::message::Message;
pub(crate) use app::model::*;
pub use app::state::{
    App, DEFAULT_FONT_SIZE, DebugState, DocsState, ExplainState, OverviewState, ProjectCallsState,
    SettingsDraft, StatsState, WalkState,
};
pub(crate) use app::tasks::*;

use iced::Size;

use crate::bookmarks::Bookmark;
use crate::fs_scan::FileEntry;
use crate::viewer::MAX_FILE_BYTES;

pub fn main() -> iced::Result {
    // A multi-window daemon: the `shell` layer holds one App per window and
    // routes each window's messages to its own App (see `crate::shell`).
    iced::daemon(shell::boot, shell::update, shell::view)
        .title(shell::title)
        .theme(shell::theme)
        .subscription(shell::subscription)
        // Embed the icon font (Nerd Font symbols) for file-type icons.
        .font(icons::FONT_BYTES)
        .run()
}

/// Frameless window settings: no OS title bar at all — clew draws its own window
/// controls (the red/amber/green buttons) in its toolbar, so dragging from a
/// toolbar button never moves the window (unlike a native full-size-content
/// title bar, which the OS drags from everywhere).
pub(crate) fn window_settings() -> iced::window::Settings {
    iced::window::Settings {
        size: Size::new(1280.0, 860.0),
        position: iced::window::Position::Centered,
        decorations: false,
        // A borderless window has square corners; we round them natively (see
        // `macos::round_corners`), which needs the window surface to carry an
        // alpha channel so the clipped-away corners composite over the desktop.
        transparent: true,
        ..iced::window::Settings::default()
    }
}
