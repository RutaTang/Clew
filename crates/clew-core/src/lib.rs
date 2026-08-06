//! clew-core: backend logic shared by the clew GUI client and the headless
//! clew-server.
//!
//! Everything here is pure computation over the filesystem — no GUI, no
//! protocol types — so the same code runs in the in-process server (linked into
//! the client) and in the standalone `clew-server` binary that runs remotely.

/// Serializes tests that mutate process-global environment variables
/// (`CLEW_DATA_DIR`, provider API keys). Cargo runs tests of one binary in
/// parallel threads, so two env-touching tests racing each other fail flakily.
/// Tolerates poisoning: a panicked test must not fail the others.
///
/// Public (not `cfg(test)`) so the GUI crate's tests, which set the same
/// variables, share this one lock — a `cfg(test)` item would be a *different*
/// lock in each crate and serialize nothing across them.
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub mod apidoc;
pub mod docs;
pub mod embed;
pub mod explain;
pub mod fs_scan;
pub mod git;
pub mod highlight;
pub mod inactive;
pub mod incremental;
pub mod llm;
pub mod lsp;
pub mod notebook;
pub mod outline;
pub mod search;
pub mod server_dist;
pub mod trust;
pub mod update;
