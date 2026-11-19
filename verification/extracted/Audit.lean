import Equal

/-! What the proofs that the code is the model rest on: `lake env lean
Audit.lean` must print only Lean's standard axioms (`propext`,
`Classical.choice`, `Quot.sound`) — no `sorry`, no `Lean.ofReduceBool`,
and none of the axioms Aeneas's library declares for the Rust functions it
does not model (the code proved here calls none of them). -/

#print axioms Clew.Extracted.Voice.route_is_the_model
#print axioms Clew.Extracted.Location.trusted_by_is_the_model
#print axioms Clew.Extracted.Framing.header_is_the_model
#print axioms Clew.Extracted.Headers.header_name_ok_is_the_model
#print axioms Clew.Extracted.Headers.header_value_ok_is_the_model
#print axioms Clew.Extracted.Shell.shell_command_is_the_model
#print axioms Clew.Extracted.Shell.sh_c_is_the_model
#print axioms Clew.Extracted.Shell.quote_is_the_model
