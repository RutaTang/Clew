import Lake
open Lake DSL

/-! Clew's formal verification: Lean models of the rules Clew's safety and
protocol correctness rest on, the theorems proved about them, and the
generator of the conformance vectors the Rust test suite checks the real
code against (`lake exe vectors vectors`). No dependency beyond Lean itself. -/

package «clew-verification» where
  leanOptions := #[⟨`autoImplicit, false⟩, ⟨`warningAsError, true⟩]

@[default_target] lean_lib Clew {}

lean_exe vectors where
  root := `Vectors
