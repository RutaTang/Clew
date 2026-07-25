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
pub(crate) use editor::{codeview, viewer, highlight, find, analyze};
mod graph;
pub(crate) use graph::{imports, projectcalls, callgraph, graphlayout, structure, index};
mod ai;
pub(crate) use ai::{overview, walkthrough, richmd, render};
mod session;
pub(crate) use session::{history, notes, bookmarks, reading, cache, finder};
mod backend;
pub(crate) use backend::{server, connect, watch, langenv};
mod miscellaneous;
pub(crate) use miscellaneous::{theme, glyph, icons, keymap, resize, stats};

mod app;
pub use app::message::Message;
pub(crate) use app::model::*;
pub(crate) use app::tasks::*;
pub use app::state::{
    App, DEFAULT_FONT_SIZE, DebugState, DocsState, ExplainState, OverviewState, ProjectCallsState,
    SettingsDraft, StatsState, WalkState,
};


use iced::Size;

use crate::bookmarks::Bookmark;
use crate::fs_scan::FileEntry;
use crate::viewer::MAX_FILE_BYTES;

pub fn main() -> iced::Result {
    // Frameless: no OS title bar at all — clew draws its own window controls
    // (the red/amber/green buttons) in its toolbar, so dragging from a toolbar
    // button never moves the window (unlike a native full-size-content title
    // bar, which the OS drags from everywhere).
    let window = iced::window::Settings {
        size: Size::new(1280.0, 860.0),
        position: iced::window::Position::Centered,
        decorations: false,
        // A borderless window has square corners; we round them natively (see
        // `macos::round_corners`), which needs the window surface to carry an
        // alpha channel so the clipped-away corners composite over the desktop.
        transparent: true,
        ..iced::window::Settings::default()
    };
    iced::application(App::new, App::update, App::view)
        .title(App::title)
        .theme(App::theme)
        .subscription(App::subscription)
        // Embed the icon font (Nerd Font symbols) for file-type icons.
        .font(icons::FONT_BYTES)
        .window(window)
        .run()
}

