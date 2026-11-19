//! Clew's pure decisions — the code the proofs in `verification/` are about.
//!
//! Each function here is translated to Lean from this very source by Charon
//! and Aeneas (`verification/extracted/`), and proved there equal to the
//! Lean model the theorems are stated over: the theorems hold of the code
//! that runs, not of a copy. So this crate stays in the part of Rust Aeneas
//! translates: no strings, no closures, no I/O, no dependencies.

pub mod approval;
pub mod confine;
pub mod fingerprint;
pub mod framing;
pub mod headers;
pub mod known_hosts;
pub mod location;
pub mod server_dist;
pub mod shell;
pub mod text;
pub mod version;
pub mod voice;
