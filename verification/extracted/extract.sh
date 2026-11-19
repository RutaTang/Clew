#!/usr/bin/env bash
# What Charon and Aeneas make of crates/clew-rules, as ClewRules.lean.
#   ./extract.sh           fail if ClewRules.lean is not what they make of the source today
#   ./extract.sh --write   write it (after changing crates/clew-rules)
# Needs `charon` and `aeneas` from the Aeneas release lakefile.lean names
# (nightly-2026.11.18-1dc8ad3) on PATH, and the Rust nightly its
# `rust-toolchain` file names, with rustc-dev (`rustup toolchain install
# nightly-2026-09-17 --component rustc-dev,llvm-tools,rust-src`).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
crate="$here/../../crates/clew-rules"

for tool in charon aeneas; do
  command -v "$tool" >/dev/null || {
    echo "::error::$tool is not on PATH — see the comment at the top of $0" >&2
    exit 1
  }
done

out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT
# The functions the app calls for its decisions, and what they call.
(cd "$crate" && charon cargo --preset=aeneas \
  --start-from crate::voice::route \
  --start-from crate::location::trusted_by \
  --start-from crate::framing::header \
  --start-from crate::headers::header_name_ok \
  --start-from crate::headers::header_value_ok \
  --start-from crate::shell::shell_command \
  --start-from crate::shell::quote \
  --start-from crate::shell::sh_c \
  --start-from crate::known_hosts::plain_host_field \
  --start-from crate::server_dist::slug_ok \
  --dest "$out" -- --target-dir "$out/target" -q) >"$out/charon.log" 2>&1 || {
  cat "$out/charon.log" >&2
  exit 1
}
aeneas -backend lean -dest "$out/lean" "$out/clew_rules.llbc" >"$out/aeneas.log" 2>&1 || {
  cat "$out/aeneas.log" >&2
  exit 1
}
if grep -q 'partial file\|\[Error\]' "$out/aeneas.log"; then
  echo "::error::Aeneas could not translate all of crates/clew-rules:" >&2
  sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' "$out/aeneas.log" | tr '\r' '\n' | grep -A4 'Error' >&2
  exit 1
fi
if grep -q '^axiom ' "$out/lean/ClewRules.lean"; then
  echo "::error::the translation leans on a Rust function Aeneas does not model:" >&2
  grep -A1 '^axiom ' "$out/lean/ClewRules.lean" >&2
  exit 1
fi

if [[ "${1:-}" == "--write" ]]; then
  cp "$out/lean/ClewRules.lean" "$here/ClewRules.lean"
  echo "wrote ClewRules.lean"
elif ! diff -u "$here/ClewRules.lean" "$out/lean/ClewRules.lean"; then
  echo "::error::ClewRules.lean is not what Aeneas makes of crates/clew-rules; run '$0 --write' and prove Equal.lean again" >&2
  exit 1
else
  echo "ClewRules.lean is what Aeneas makes of crates/clew-rules"
fi
