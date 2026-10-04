#!/usr/bin/env bash
# Assemble Clew.app — a real macOS app bundle with both binaries and the icon.
#
# The GUI (clew) and the backend (clew-server) both land in Contents/MacOS/, so
# the client's sibling-binary lookup (server_bin_path) finds the server inside
# the bundle with no path config. Ad-hoc signed so it runs locally; Developer ID
# signing + notarization is a separate step for distributing to other machines.
#
# Usage: scripts/build-app.sh [--flavor prod|dev|test] [--version X.Y.Z] [--build N] [--debug]
#   --version  the marketing version (CFBundleShortVersionString); default: the
#              crate version in Cargo.toml. The release passes the tag's, which
#              CI checks equals the crate version (the updater relies on it).
#   --build    the build number (CFBundleVersion); default 1.
#
# Builds with `--locked`: the bundle is built from exactly the dependency
# versions in Cargo.lock, never from whatever resolves today. A release build
# may set CLEW_SERVER_DIGESTS (see .github/workflows/release.yml), which cargo
# passes through to the compiler and clew embeds.
# Python 3's standard library reads Cargo's executable artifact paths, honoring
# Cargo's environment and configured output directory.
#
# Flavors get distinct bundle ids AND names so prod / dev / test can be
# installed and run side by side without colliding:
#   prod  com.rutatang.clew        Clew.app        (default)
#   dev   com.rutatang.clew.dev    Clew Dev.app
#   test  com.rutatang.clew.test   Clew Test.app
set -euo pipefail
cd "$(dirname "$0")/.."

FLAVOR="prod"
PROFILE="release"
CARGO_ARGS=(build --release)
# Marketing version defaults to the crate version; the build number to 1. A
# release overrides both (e.g. --version 1.2.0 --build 42 from the tag / run).
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
BUILD="1"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --flavor)  FLAVOR="$2"; shift 2;;
    --version) VERSION="$2"; shift 2;;
    --build)   BUILD="$2"; shift 2;;
    --debug)   PROFILE="debug"; CARGO_ARGS=(build); shift;;
    *) echo "unknown argument: $1" >&2; exit 1;;
  esac
done

case "$FLAVOR" in
  prod) BUNDLE_ID="com.rutatang.clew";      APP_NAME="Clew";;
  dev)  BUNDLE_ID="com.rutatang.clew.dev";  APP_NAME="Clew Dev";;
  test) BUNDLE_ID="com.rutatang.clew.test"; APP_NAME="Clew Test";;
  *) echo "unknown flavor: $FLAVOR (want prod|dev|test)" >&2; exit 1;;
esac

APP="dist/$APP_NAME.app"

# Cargo chooses its output directory from environment and configuration (and
# may add a target triple). Use the artifact it actually built, rather than
# guessing a path that could still contain an older build.
ARTIFACTS="$(mktemp)"
trap 'rm -f "$ARTIFACTS"' EXIT
build_binary() {
  local package="$1" binary="$2"
  cargo "${CARGO_ARGS[@]}" --locked -p "$package" --bin "$binary" \
    --message-format=json-render-diagnostics > "$ARTIFACTS" || return
  python3 - "$binary" "$ARTIFACTS" <<'PY'
import json
import sys
from pathlib import Path

binary, messages = sys.argv[1:]
paths = set()
with open(messages, encoding="utf-8") as stream:
    for line in stream:
        message = json.loads(line)
        target = message.get("target", {})
        if (message.get("reason") == "compiler-artifact"
                and target.get("name") == binary
                and "bin" in target.get("kind", [])
                and message.get("executable")):
            paths.add(message["executable"])
if len(paths) != 1:
    sys.exit(f"expected one executable artifact for {binary}, found {len(paths)}")
print(Path(paths.pop()).resolve(strict=True))
PY
}

echo "==> Building clew + clew-server ($PROFILE, flavor=$FLAVOR)"
CLEW_BIN="$(build_binary clew clew)"
SERVER_BIN="$(build_binary clew-server clew-server)"

# Regenerate the icon if the vector toolchain is present; otherwise use the
# committed assets/clew.icns.
if command -v resvg >/dev/null 2>&1 && [[ -f assets/icon/clew.svg ]]; then
  ./scripts/gen-icon.sh
fi

echo "==> Assembling $APP  ($BUNDLE_ID)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$CLEW_BIN"   "$APP/Contents/MacOS/clew"
cp "$SERVER_BIN" "$APP/Contents/MacOS/clew-server"
cp assets/clew.icns       "$APP/Contents/Resources/clew.icns"

# License notices travel with the binaries: the bundled font's license and the
# texts it refers to, and every Rust crate compiled into clew and clew-server.
LICENSES="$APP/Contents/Resources/Licenses"
mkdir -p "$LICENSES"
cp assets/NERDFONT-LICENSE.md "$LICENSES/"
cp -R assets/licenses "$LICENSES/licenses"
./scripts/third-party-notices.sh "$LICENSES/THIRD-PARTY-NOTICES.txt" clew clew-server

# Fill the plist template for this flavor and version.
sed -e "s|__APP_NAME__|$APP_NAME|g" -e "s|__BUNDLE_ID__|$BUNDLE_ID|g" \
    -e "s|__VERSION__|$VERSION|g" -e "s|__BUILD__|$BUILD|g" \
  assets/Info.plist.template > "$APP/Contents/Info.plist"

# Ad-hoc sign (identity "-") so Gatekeeper lets it run on this machine. Sign the
# nested binary first, then the bundle, so the outer signature seals it.
echo "==> Ad-hoc signing"
codesign --force -s - "$APP/Contents/MacOS/clew-server"
codesign --force -s - "$APP/Contents/MacOS/clew"
codesign --force -s - "$APP"

echo "==> Done: $APP"
codesign -dv "$APP" 2>&1 | grep -E 'Identifier|Signature' || true
