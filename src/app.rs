//! `App`'s update / message-handling logic, split out of `main.rs` by area.
//!
//! Each submodule is one or more `impl App` blocks; the `App` type itself, its
//! per-domain state sub-structs and the `Message` enum stay in the crate root
//! (`main.rs`). Methods are `pub(crate)` so the dispatcher and siblings can call
//! across module boundaries.

mod prelude;

pub(crate) mod message;
pub(crate) mod state;

mod content;
mod handlers_actions;
mod handlers_features;
mod navigation;
mod runtime;
mod server_ai;
mod services;
mod session;
mod update;
