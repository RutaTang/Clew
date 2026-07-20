#!/usr/bin/env bash
# Build clew.icns from the vector source (assets/icon/clew.svg).
# Renders each iconset size straight from the SVG (crisper than downscaling one
# PNG), then packs them with iconutil. Requires: resvg, iconutil (macOS).
set -euo pipefail
cd "$(dirname "$0")/.."

SRC="assets/icon/clew.svg"
ICONSET="$(mktemp -d)/clew.iconset"
mkdir -p "$ICONSET"

# (size, filename) pairs Apple's iconutil expects.
render() { resvg -w "$1" "$SRC" "$ICONSET/$2" >/dev/null; }
render 16   icon_16x16.png
render 32   icon_16x16@2x.png
render 32   icon_32x32.png
render 64   icon_32x32@2x.png
render 128  icon_128x128.png
render 256  icon_128x128@2x.png
render 256  icon_256x256.png
render 512  icon_256x256@2x.png
render 512  icon_512x512.png
render 1024 icon_512x512@2x.png

iconutil -c icns "$ICONSET" -o assets/clew.icns
echo "wrote assets/clew.icns"
