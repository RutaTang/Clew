#!/usr/bin/env bash
# Clew's formal verification, checked end to end:
#   1. every proof (`lake build`; a `sorry` fails it — warnings are errors);
#   2. what the proofs rest on: Lean's three standard axioms and nothing
#      else (Audit.lean) — no `sorry`, no `Lean.ofReduceBool`;
#   3. the committed conformance vectors are exactly what the models
#      compute today, so a model change cannot leave the Rust tests checking
#      against an older one.
# The Rust side — the real functions run on those vectors — is part of
# `cargo test`.
set -euo pipefail
cd "$(dirname "$0")"

lake build

audit="$(lake env lean Audit.lean)"
printf '%s\n' "$audit"
expected=$(grep -c '^#print axioms' Audit.lean)
checked=$(grep -c ' axioms' <<<"$audit" || true)
if [[ "$checked" -ne "$expected" ]]; then
  echo "::error::the axiom audit printed $checked results for $expected theorems" >&2
  exit 1
fi
standard='^.(propext|Classical\.choice|Quot\.sound)(, (propext|Classical\.choice|Quot\.sound))*.$'
while IFS= read -r line; do
  case "$line" in
    *"does not depend on any axioms"*) ;;
    *"depends on axioms: "*)
      axioms="${line#*depends on axioms: }"
      if ! grep -Eq "$standard" <<<"$axioms"; then
        echo "::error::a proof rests on more than Lean's standard axioms: $line" >&2
        exit 1
      fi
      ;;
    *)
      echo "::error::unexpected audit output: $line" >&2
      exit 1
      ;;
  esac
done <<<"$audit"

fresh="$(mktemp -d)"
trap 'rm -rf "$fresh"' EXIT
lake exe vectors "$fresh"
if ! diff -r vectors "$fresh" >/dev/null; then
  echo "::error::verification/vectors is not what the models compute; run 'lake exe vectors vectors' and commit it" >&2
  diff -r vectors "$fresh" | head -20 >&2
  exit 1
fi
echo "verification: proofs check, axioms standard, vectors current"
