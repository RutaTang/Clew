//! Clew's plugin system. docs/plugins.md is the design; GUIDE.md is what an
//! agent reads to write a plugin; wit/plugin.wit is the contract between
//! the two sides.
//!
//! - [`manifest`] reads `plugin.toml`.
//! - [`build`] builds a plugin from its source, with the Rust
//!   [`toolchain`] finds or sets up, checking first that what is compiled
//!   reads nothing of the computer's but its own sources (`reads`).
//! - [`mod@inspect`] reads a plugin's file without running it: its manifest,
//!   imports, exports and the permissions they need.
//! - [`store`] keeps what the user approved, and the copies Clew runs.
//! - [`authoring`], [`tools`] and [`requests`] are what agents make plugins
//!   with.
//! - `runtime` (a feature) runs plugins.

pub mod authoring;
pub mod build;
pub mod inspect;
pub mod manifest;
mod reads;
pub mod requests;
#[cfg(feature = "runtime")]
pub mod runtime;
pub mod store;
pub mod toolchain;
pub mod tools;

pub use inspect::{Inspected, inspect};
pub use manifest::Manifest;
