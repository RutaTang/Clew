//! Whether hovering an identifier in the paused program may evaluate it:
//! `hover_eval_allowed` in `src/dap/proto.rs` is this.
//!
//! That a hover evaluates only when the adapter promised it runs nothing, or
//! you chose to let it, is proved in `verification/Clew/Guards.lean`, and the
//! Lean Aeneas makes of this source is proved to be that model.

/// A hover is not a request to run anything, yet an evaluation can run the
/// program's own code (property getters, `Debug`/`toString` impls). So it
/// happens only when the adapter promised a side-effect-free `"hover"`
/// evaluation (`supportsEvaluateForHovers`), or the reader opted in.
pub fn hover_eval_allowed(adapter_promises_no_side_effects: bool, opted_in: bool) -> bool {
    adapter_promises_no_side_effects || opted_in
}
