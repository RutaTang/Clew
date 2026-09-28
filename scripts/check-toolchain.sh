#!/usr/bin/env bash
# Fail unless every place that names the Rust toolchain agrees:
#   - rust-toolchain.toml's `channel` (what cargo actually runs, via rustup),
#   - the version the CI toolchain step installed (passed as $1),
#   - every `dtolnay/rust-toolchain@<commit> # <version>` pin in the workflows,
#   - `rust-version` (the MSRV) in every crate's Cargo.toml.
#
# CI denies warnings, so a toolchain that drifts on its own turns every branch
# red the day a new clippy lint ships; pinning only works if the pins agree.
#
# Usage: scripts/check-toolchain.sh <installed-version>
set -euo pipefail
cd "$(dirname "$0")/.."

INSTALLED="${1:?usage: check-toolchain.sh <version the toolchain step installed>}"
CHANNEL="$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)"
if [[ -z "$CHANNEL" ]]; then
  echo "::error file=rust-toolchain.toml::no channel pinned"
  exit 1
fi
fail=0

if [[ "$INSTALLED" != "$CHANNEL" ]]; then
  echo "::error::CI installed Rust $INSTALLED but rust-toolchain.toml pins $CHANNEL — bump them together"
  fail=1
fi

# rustc as cargo will run it: rustup resolves rust-toolchain.toml.
ACTUAL="$(rustc --version | awk '{print $2}')"
if [[ "$ACTUAL" != "$CHANNEL" ]]; then
  echo "::error::rustc reports $ACTUAL, but rust-toolchain.toml pins $CHANNEL"
  fail=1
fi

# Every pin of the toolchain action names the same version.
while IFS= read -r line; do
  if [[ "$line" != *"# $CHANNEL" ]]; then
    echo "::error::toolchain pin does not say $CHANNEL: $line"
    fail=1
  fi
done < <(grep -rn 'dtolnay/rust-toolchain@' .github/workflows)

# The MSRV every crate declares is the pinned channel's major.minor.
MSRV="${CHANNEL%.*}"
for manifest in Cargo.toml crates/*/Cargo.toml; do
  if ! grep -q "^rust-version = \"$MSRV\"$" "$manifest"; then
    echo "::error file=$manifest::rust-version must be \"$MSRV\" (rust-toolchain.toml pins $CHANNEL)"
    fail=1
  fi
done

if [[ "$fail" -ne 0 ]]; then
  exit 1
fi
echo "Rust $CHANNEL: rust-toolchain.toml, the CI pins, rustc and every rust-version agree"
