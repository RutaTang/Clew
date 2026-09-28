//! Debug Adapter Protocol (DAP) support: clew acts as a DAP *client*, driving an
//! external debug adapter (lldb-dap for Rust/C/C++, debugpy for Python, …) the
//! same way [`crate::lsp`] drives a language server. The wire framing is
//! identical (`Content-Length` JSON, [`clew_core::framing`]); only the payload
//! semantics differ (request/response + adapter events).
//!
//! Adapter resolution and provisioning are shared with clew-server in
//! [`clew_core::debugadapter`]; [`adapter`] adds the client's language
//! detection on top.

pub mod adapter;
pub mod client;
pub mod proto;

pub use adapter::{AdapterInstall, Lang, Resolved};
pub use client::DapClient;
pub use proto::{
    Breakpoint, DapEvent, EvalContext, StackFrame, Variable, hover_eval_allowed,
    promises_hover_eval,
};
