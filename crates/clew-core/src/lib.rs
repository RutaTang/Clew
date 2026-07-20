//! clew-core: backend logic shared by the clew GUI client and the headless
//! clew-server.
//!
//! Everything here is pure computation over the filesystem — no GUI, no
//! protocol types — so the same code runs in the in-process server (linked into
//! the client) and in the standalone `clew-server` binary that runs remotely.

pub mod fs_scan;
pub mod search;
