//! clew-core: backend logic shared by the clew GUI client and the headless
//! clew-server.
//!
//! Everything here is pure computation over the filesystem — no GUI, no
//! protocol types — so the same code runs in the in-process server (linked into
//! the client) and in the standalone `clew-server` binary that runs remotely.

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
pub mod outline;
pub mod search;
pub mod server_dist;
pub mod update;
