//! LSP support: clew manages its own version-pinned language servers.
//!
//! Layers:
//! - [`registry`]: the table of built-in, version-pinned servers and their
//!   verified downloads / toolchain installs.
//! - [`config`]: per-project `.clew/lsp.toml` and effective-server resolution.
//! - [`store`]: the global server store and consent-gated provisioning, which
//!   unpacks through `archive` (confined, budgeted extraction).
//! - [`client`]: the LSP client, on the shared [`crate::framing`] transport.
//! - [`langenv`]: the project's language environment (its Python venv),
//!   handed to servers at `initialize` and to the Python debugger.

mod archive;
pub mod client;
pub mod config;
pub mod langenv;
pub mod registry;
pub mod store;
