//! Project-wide Rust type structure — the index and its build live in
//! `clew_core::structure` (shared with clew-server, which ships the index in
//! the remote project snapshot); re-exported so `crate::structure::*` paths
//! keep working.

pub use clew_core::structure::{StructureIndex, build};
