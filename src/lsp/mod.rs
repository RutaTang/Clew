//! LSP support: clew manages its own version-pinned language servers.
//!
//! Layers:
//! - [`registry`]: built-in, version-pinned server specs + download metadata.
//! - [`config`]: per-project `.clew/lsp.toml` and effective-server resolution.
//! - [`store`]: the global server store and consent-gated provisioning.

pub mod client;
pub mod config;
pub mod registry;
pub mod store;
