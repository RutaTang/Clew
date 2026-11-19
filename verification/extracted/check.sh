#!/usr/bin/env bash
# The code that runs is the model, checked end to end:
#   1. ClewRules.lean is what Charon and Aeneas make of crates/clew-rules
#      today (extract.sh) — the Lean proved about is the Rust that runs;
#   2. every proof that it is the model checks (`lake build`; warnings are
#      errors, so no `sorry`);
#   3. what those proofs rest on: Lean's three standard axioms alone.
set -euo pipefail
cd "$(dirname "$0")"

./extract.sh
lake build

audit="$(lake env lean Audit.lean)"
printf '%s\n' "$audit"
expected=$(grep -c '^#print axioms' Audit.lean)
standard="depends on axioms: \[(propext|Classical\.choice|Quot\.sound)(, (propext|Classical\.choice|Quot\.sound))*\]$"
if [[ "$(grep -Ec "$standard" <<<"$audit")" -ne "$expected" ]]; then
  echo "::error::a proof that the code is the model rests on more than Lean's standard axioms" >&2
  exit 1
fi
echo "extracted: the Rust is the model, on standard axioms"
