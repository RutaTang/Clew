#!/usr/bin/env bash
# Run `cargo test` with the given arguments and fail when the tests leave a
# trace outside what they own:
#   * the temp dir — a fresh one for the run, which must be empty afterwards
#     (every test removes what it created, a failing one included);
#   * HOME — a fresh, empty one for the run: a test that wrote there (the real
#     data directory's fallback, a config file, a cache) would otherwise pass
#     while writing into the developer's or runner's home;
#   * the checkout — every path git reports as changed, untracked or IGNORED
#     (the build's own `target/` aside), each with a hash of its content,
#     must read the same after the run as before it: a test that wrote into
#     the source tree fails, including one that wrote project state into an
#     ignored `.clew/` or rewrote a file that was already modified.
#
# Cargo and rustup keep their own homes: they are pinned before HOME moves.
#
# Usage: scripts/hermetic-test.sh [cargo test arguments…]
set -euo pipefail

scratch="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/clew-hermetic.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/tmp" "$scratch/home"

export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
# The checkout's state. Runs under the real HOME, so git reads the real
# configuration; the tests pin their own. `--ignored` matters: `.gitignore`
# hides `.clew/` at any depth, which is exactly where the product writes a
# project's state. The hashes catch a second write to an already-dirty file,
# which leaves its status line as it was.
status() {
  git status --porcelain=v1 --untracked-files=all --ignored -- ':/' ':(top,exclude)target'
  local files
  files="$(git ls-files -z --modified --others -- ':/' ':(top,exclude)target' |
    while IFS= read -r -d '' path; do
      if [ -f "$path" ]; then printf '%s\n' "$path"; fi
    done)"
  if [ -n "$files" ]; then
    paste -d ' ' <(printf '%s\n' "$files" | git hash-object --stdin-paths) \
      <(printf '%s\n' "$files")
  fi
}
before="$(status)"

HOME="$scratch/home" TMPDIR="$scratch/tmp/" cargo test "$@"

failed=0
leftovers="$(ls -A "$scratch/tmp")"
if [ -n "$leftovers" ]; then
  echo "::error::the tests left entries in their temp dir:"
  echo "$leftovers"
  failed=1
fi
home="$(ls -A "$scratch/home")"
if [ -n "$home" ]; then
  echo "::error::the tests wrote into HOME:"
  (cd "$scratch/home" && find . -mindepth 1 | head -50)
  failed=1
fi
after="$(status)"
if [ "$before" != "$after" ]; then
  echo "::error::the tests changed the checkout:"
  diff <(echo "$before") <(echo "$after") || true
  failed=1
fi
exit "$failed"
