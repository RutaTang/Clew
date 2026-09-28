#!/usr/bin/env bash
# Write the license notices of every third-party crate compiled into the given
# clew packages: one section per crate (name, version, SPDX license expression,
# repository) followed by the crate's own LICENSE / COPYING / NOTICE /
# COPYRIGHT files from the cargo registry.
#
# MIT and Apache-2.0 — the licenses of nearly every dependency — require their
# notices to travel with binary copies, and clew ships binaries: the .app (via
# scripts/build-app.sh) and the Linux clew-server release assets (via
# .github/workflows/release.yml).
#
# Usage: scripts/third-party-notices.sh <output-file> [--target <triple>] <package>...
#   e.g. scripts/third-party-notices.sh dist/NOTICES.txt clew clew-server
#
# Offline: it reads `cargo tree` (normal dependencies only — build tools and
# dev-dependencies are not distributed) and the already-fetched registry
# sources, so run it after the build it describes.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="${1:?usage: third-party-notices.sh <output-file> [--target <triple>] <package>...}"
shift
TREE_ARGS=(-e normal --prefix none --format '{p}|{l}|{r}' --locked)
if [[ "${1:-}" == "--target" ]]; then
  TREE_ARGS+=(--target "${2:?--target needs a triple}")
  shift 2
fi
[[ $# -gt 0 ]] || { echo "no packages given" >&2; exit 1; }
PACKAGES=()
for p in "$@"; do PACKAGES+=(-p "$p"); done

REGISTRY="${CARGO_HOME:-$HOME/.cargo}/registry/src"
TMP="$(mktemp)"
# Texts already written, keyed by checksum: hundreds of crates ship the very
# same Apache-2.0 or MIT file, and each distinct text is printed once.
SEEN="$(mktemp -d)"
trap 'rm -rf "$TMP" "$SEEN"' EXIT

# One line per crate: "name vX.Y.Z|license|repository". Repeated subtrees are
# marked " (*)"; path dependencies (clew's own crates) carry " (<path>)" in the
# package column and are skipped.
cargo tree "${PACKAGES[@]}" "${TREE_ARGS[@]}" \
  | sed 's/ (\*)$//' \
  | sort -u \
  | awk -F'|' '$1 !~ / \(/' > "$TMP"

{
  echo "Third-party software distributed with: $*"
  echo
  echo "Each section names a crate compiled into these binaries, its license as"
  echo "declared in its manifest, and the license files it ships with."
  missing=0
  while IFS='|' read -r pkg license repo; do
    name="${pkg%% *}"
    version="${pkg#* v}"
    echo
    echo "================================================================================"
    echo "$name $version"
    echo "License: ${license:-(not declared)}"
    [[ -n "$repo" ]] && echo "Repository: $repo"
    found=0
    for dir in "$REGISTRY"/*/"$name-$version"; do
      [[ -d "$dir" ]] || continue
      for file in "$dir"/LICENSE* "$dir"/LICENCE* "$dir"/COPYING* "$dir"/NOTICE* "$dir"/COPYRIGHT*; do
        [[ -f "$file" ]] || continue
        found=1
        key="$SEEN/$(cksum < "$file" | tr -c '0-9\n' '-')"
        echo "--------------------------------------------------------------------------------"
        if [[ -e "$key" ]]; then
          echo "${file##*/}: same text as $(cat "$key")"
        else
          echo "the ${file##*/} of $name $version" > "$key"
          echo "${file##*/}:"
          echo
          cat "$file"
        fi
      done
      break
    done
    if [[ "$found" -eq 0 ]]; then
      echo "(no license file in the crate; see its repository)"
      missing=$((missing + 1))
    fi
  done < "$TMP"
} > "$OUT"

echo "wrote $OUT ($(wc -l < "$TMP" | tr -d ' ') crates, $missing without a license file of their own)"
