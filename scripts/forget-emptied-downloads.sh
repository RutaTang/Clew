#!/usr/bin/env bash
# Run after Swatinem/rust-cache restores target/, before anything builds.
#
# rust-cache keeps the folders of target/ it does not know as cargo profiles,
# but empties them: it removes their files and keeps the folders.
# sherpa-onnx-sys downloads its prebuilt libraries into
# target/sherpa-onnx-prebuilt/<archive>/lib and, finding that folder there,
# downloads nothing — so with a restored cache, a build that runs its build
# script again (a dependency of it changed, say) fails to link: "could not
# find native static library `sherpa-onnx-c-api`". Removed, the libraries are
# downloaded again when its build script runs; a sherpa-onnx-sys already
# built carries them in its own rlib and needs none of it.
set -euo pipefail
rm -rf target/sherpa-onnx-prebuilt
