//! clew-plugin's integration tests, in one test binary (each links all of
//! clew-core, so one is much smaller than three):
//!
//! - `sdk_bindings`: the guest SDK's bindings are the WIT's.
//! - `fixtures`: the test plugins are built from their sources (and
//!   `--ignored`, builds them).
//! - `runtime`: the runtime, running them.

mod fixtures;
#[cfg(feature = "runtime")]
mod runtime;
mod sdk_bindings;
