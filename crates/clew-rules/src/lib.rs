//! Clew's pure decisions — the code the proofs in `verification/` are about.
//!
//! Each function here is translated to Lean from this very source by Charon
//! and Aeneas (`verification/extracted/`), and proved there equal to the
//! Lean model the theorems are stated over: the theorems hold of the code
//! that runs, not of a copy. So this crate stays in the part of Rust Aeneas
//! translates: no strings, no closures, no I/O, no dependencies.

pub mod framing;
pub mod location;
pub mod voice;
