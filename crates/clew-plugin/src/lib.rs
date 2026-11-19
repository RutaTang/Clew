//! Clew's plugin system. docs/plugins.md is the design; GUIDE.md is what an
//! agent reads to write a plugin; wit/plugin.wit is the contract between
//! the two sides.
//!
//! - [`manifest`] reads `plugin.toml`.
//! - [`mod@inspect`] reads a plugin's file without running it: its manifest,
//!   imports, exports and the permissions they need.
//! - [`store`] keeps what the user approved, and the copies Clew runs.
//! - `runtime` (a feature) runs plugins.

pub mod inspect;
pub mod manifest;
pub mod store;

pub use inspect::{Inspected, inspect};
pub use manifest::Manifest;
