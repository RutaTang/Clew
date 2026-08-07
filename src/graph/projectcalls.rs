//! Project-wide call graph — the build lives in `clew_core::projectcalls`
//! (shared with clew-server, which answers `ProjectCalls` for remote
//! projects); re-exported so `crate::projectcalls::*` paths keep working.

pub use clew_core::projectcalls::*;
