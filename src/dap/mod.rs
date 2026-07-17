//! Debug Adapter Protocol (DAP) support: clew acts as a DAP *client*, driving an
//! external debug adapter (lldb-dap for Rust/C/C++, debugpy for Python, …) the
//! same way [`crate::lsp`] drives a language server. The wire framing is
//! identical (`Content-Length` JSON over stdio); only the payload semantics
//! differ (request/response + adapter events).
//!
//! The transport/engine layer was validated against lldb-dap on a Rust binary;
//! the App wiring + debugger UI consume it.

pub mod adapter;
pub mod client;
pub mod proto;
pub mod provision;

pub use adapter::Lang;
pub use client::DapClient;
pub use proto::{DapEvent, StackFrame, Variable};
