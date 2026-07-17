//! Debug Adapter Protocol (DAP) support: clew acts as a DAP *client*, driving an
//! external debug adapter (lldb-dap for Rust/C/C++, debugpy for Python, …) the
//! same way [`crate::lsp`] drives a language server. The wire framing is
//! identical (`Content-Length` JSON over stdio); only the payload semantics
//! differ (request/response + adapter events).
//!
//! This is the transport/engine layer (validated against lldb-dap on a Rust
//! binary). The App wiring and debugger UI consume it next; until then some of
//! its surface is unused.
#![allow(dead_code, unused_imports)]

pub mod client;
pub mod proto;

pub use client::DapClient;
pub use proto::{Breakpoint, DapEvent, Output, Scope, StackFrame, Stopped, Variable};
