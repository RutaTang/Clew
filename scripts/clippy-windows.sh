#!/usr/bin/env bash
# Lint the whole workspace for Windows from Linux, warnings denied: every
# crate and every test, as the Windows CI job builds them, without a Windows
# machine.
#
# Needs the target (`rustup target add x86_64-pc-windows-gnu`) and the MinGW
# headers (Debian/Ubuntu: `apt-get install mingw-w64`). Nothing is linked and
# no C is compiled: whisper.cpp's sources are skipped (DOCS_RS, which
# whisper-rs-sys honours), and its Rust bindings are generated for Windows
# from MinGW's headers, since the ones it ships are Linux's (their struct
# sizes differ).
#
# Usage: scripts/clippy-windows.sh [extra cargo clippy arguments]
set -euo pipefail
cd "$(dirname "$0")/.."

mingw=/usr/x86_64-w64-mingw32/include
[[ -d "$mingw" ]] || {
  echo "error: no MinGW headers at $mingw (install mingw-w64)" >&2
  exit 1
}

export DOCS_RS=1
export BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="-I$mingw"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/windows-check}"
exec cargo clippy --target x86_64-pc-windows-gnu --workspace --all-targets "$@" -- -D warnings
