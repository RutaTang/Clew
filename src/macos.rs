//! Native AppKit integration (macOS only): the frameless-window chrome tweaks
//! and the top-of-screen menu bar. Split into submodules; `round_corners` is
//! re-exported so `crate::macos::round_corners` keeps working.

pub mod appearance;
pub mod menu;
pub mod window;

pub use window::{minimize_key_window, round_corners};
