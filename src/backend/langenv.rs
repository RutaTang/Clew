//! The per-project language environment merged into a language server's
//! `initializationOptions` at launch. It lives in [`clew_core::lsp::langenv`],
//! shared with debug-adapter resolution, so a Python program is debugged under
//! the same interpreter its language server was pointed at; the app reaches it
//! through this path.

pub use clew_core::lsp::langenv::merge;
