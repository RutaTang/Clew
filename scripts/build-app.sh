#!/usr/bin/env bash
# Assemble clew.app — a real macOS app bundle with both binaries and the icon.
#
# The GUI (clew) and the backend (clew-server) both land in Contents/MacOS/, so
# the client's sibling-binary lookup (server_bin_path) finds the server inside
# the bundle with no path config. Ad-hoc signed so it runs locally; Developer ID
# signing + notarization is a separate step for distributing to other machines.
#
# Usage: scripts/build-app.sh [--debug]   (default: release)
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="release"
CARGO_FLAGS="--release"
if [[ "${1:-}" == "--debug" ]]; then
  PROFILE="debug"
  CARGO_FLAGS=""
fi

APP="dist/clew.app"
BIN_DIR="target/$PROFILE"

echo "==> Building clew + clew-server ($PROFILE)"
cargo build $CARGO_FLAGS --bin clew
cargo build $CARGO_FLAGS -p clew-server --bin clew-server

# Regenerate the icon if the vector toolchain is present; otherwise use the
# committed assets/clew.icns.
if command -v resvg >/dev/null 2>&1 && [[ -f assets/icon/clew.svg ]]; then
  ./scripts/gen-icon.sh
fi

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN_DIR/clew"        "$APP/Contents/MacOS/clew"
cp "$BIN_DIR/clew-server" "$APP/Contents/MacOS/clew-server"
cp assets/Info.plist      "$APP/Contents/Info.plist"
cp assets/clew.icns       "$APP/Contents/Resources/clew.icns"

# Ad-hoc sign (identity "-") so Gatekeeper lets it run on this machine. Sign the
# nested binary first, then the bundle, so the outer signature seals it.
echo "==> Ad-hoc signing"
codesign --force -s - "$APP/Contents/MacOS/clew-server"
codesign --force -s - "$APP/Contents/MacOS/clew"
codesign --force -s - "$APP"

echo "==> Done: $APP"
codesign -dv "$APP" 2>&1 | grep -E 'Identifier|Signature' || true
