//! The application: its model, messages and update logic.
//!
//! The `App` type lives in `state`, with the two structs whose lifetimes it
//! is built around: the `ProjectSession` holding everything scoped to the open
//! project (replaced whole when the project is left or the transport
//! switches) and, in `server`, the `ServerLink` to the window's clew-server
//! (replaced whole when the transport dies). The `Message` enum lives in
//! `message`, nested by feature (`Message::Ask(AskMsg)`, …); the shared domain
//! types in `model`; the RPC client the AI and git flows use in `rpc`; the
//! async task bodies in `tasks`. `main.rs` re-exports them so `crate::App` /
//! `crate::Message` paths hold. `update` checks every async message once and
//! routes each sub-enum to its feature; the remaining submodules are `impl
//! App` blocks, one per feature, each with its `update_<feature>` and its
//! handlers, `pub(crate)` so features can call across module boundaries:
//!
//! - `runtime` — construction, the iced hooks, window chrome, the refresh tick
//! - `settings` — the LLM settings modal, appearance, the cached AI config
//! - `project` — consent, scanning, installing and leaving a project, stamps
//! - `server` — the server link, the transport lifecycle, event routing
//! - `connect` — the Connect modal and switching transport
//! - `remote_state` — a remote project's `.clew/` state over the protocol
//! - `reading` — trail, bookmarks, notes and the reading target
//! - `editor` — the code panes: open, load, reload, select, fold, find, diff
//! - `keys` — keyboard handling and the command keymap
//! - `hover` — the Cmd-hover peek and the context menu
//! - `navigation` — go-to / references, search, the finder
//! - `calls` — the Calls tab's call hierarchy
//! - `graph` — the import and call graphs and their overlays
//! - `watch` — watcher batches and the incremental index
//! - `explain` — the explain pass and the explanation overlay
//! - `content` — rich content (markdown, math, mermaid) and the caret context
//! - `overview` — the overview and stats homes
//! - `walkthrough` — generating, saving and reading tours
//! - `ask` — the Ask panel: agent turns, retrieval answers, streaming
//! - `semantic` — the embedding index and semantic search
//! - `timetravel` — git time travel and "Why is this here?"
//! - `lsp` — language servers and the Language Servers panel
//! - `debug` — the DAP debugger
//! - `docs` — the DOCS tab
//! - `tutorial`, `updater` — the guided tour and auto-update

mod prelude;

pub(crate) mod model;
pub(crate) mod rpc;
pub(crate) mod tasks;

pub(crate) mod message;
pub(crate) mod state;

mod ask;
mod calls;
mod connect;
mod content;
mod debug;
mod docs;
mod editor;
mod explain;
mod graph;
mod hover;
mod keys;
mod lsp;
mod navigation;
mod overview;
mod project;
mod reading;
mod remote_state;
mod runtime;
mod semantic;
pub(crate) mod server;
mod settings;
mod timetravel;
pub(crate) mod tutorial;
mod update;
mod updater;
mod walkthrough;
mod watch;

// The edits a closing window would lose, which the shell asks the user about
// before it lets the window go (`crate::shell`).
pub(crate) use remote_state::{UnsavedEdits, close_question, quit_question};

// A summary as the views outside the explanation overlay show it (`crate::ui`).
pub(crate) use explain::shown_summary;

// The mark it carries when it was kept unchecked, for the views' tests.
#[cfg(test)]
pub(crate) use explain::UNCHECKED_MARK;

#[cfg(test)]
pub(crate) mod tests;
