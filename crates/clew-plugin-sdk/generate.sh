#!/bin/sh
# Regenerate src/bindings.rs from crates/clew-plugin/wit/plugin.wit, with the
# wit-bindgen this crate's runtime (src/rt.rs) follows:
#
#   cargo install wit-bindgen-cli --version 0.57.1 --locked
#   crates/clew-plugin-sdk/generate.sh
#
# The first line of the file records the digest of the WIT it came from
# (carriage returns left out, so a Windows checkout agrees); a test in
# clew-plugin fails when the WIT has changed since.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
wit="$here/../clew-plugin/wit"
bindgen=${WIT_BINDGEN:-wit-bindgen}
case $("$bindgen" --version) in
    "wit-bindgen-cli 0.57.1"*) ;;
    *) echo "generate.sh: needs wit-bindgen-cli 0.57.1 (WIT_BINDGEN=path to use another binary)" >&2; exit 1 ;;
esac
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
"$bindgen" rust "$wit" --world plugin --out-dir "$out" --format \
    --runtime-path crate::rt --pub-export-macro \
    --default-bindings-module clew_plugin_sdk::bindings >/dev/null
if command -v sha256sum >/dev/null; then
    digest=$(tr -d '\r' < "$wit/plugin.wit" | sha256sum | cut -d' ' -f1)
else
    digest=$(tr -d '\r' < "$wit/plugin.wit" | shasum -a 256 | cut -d' ' -f1)
fi
{
    printf '// clew: generated from crates/clew-plugin/wit/plugin.wit, sha256 %s\n' "$digest"
    cat "$out/plugin.rs"
} > "$here/src/bindings.rs"
