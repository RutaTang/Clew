//! Native AppKit integration (macOS only): the frameless-window chrome tweaks
//! and the top-of-screen menu bar. Split into submodules; `configure_frameless`
//! is re-exported so `crate::macos::configure_frameless` keeps working.

pub mod appearance;
pub mod install;
pub mod menu;
pub mod window;

pub use window::{configure_frameless, minimize_key_window};
