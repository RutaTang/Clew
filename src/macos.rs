//! Native AppKit integration (macOS only): the frameless-window chrome tweaks,
//! what the shell's questions need of a window, the top-of-screen menu bar,
//! and the answer to macOS's own quit requests.
//! Split into submodules; `configure_frameless` is re-exported so
//! `crate::macos::configure_frameless` keeps working.

pub mod appearance;
pub mod install;
pub mod menu;
pub mod terminate;
pub mod window;

pub use window::{configure_frameless, minimize_key_window};
