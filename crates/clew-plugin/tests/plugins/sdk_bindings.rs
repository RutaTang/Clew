//! The guest SDK's bindings are generated from wit/plugin.wit by
//! crates/clew-plugin-sdk/generate.sh, which records the WIT's digest on the
//! first line. A WIT changed without regenerating would leave plugins built
//! against a contract Clew no longer speaks.

use sha2::{Digest, Sha256};

#[test]
fn the_sdk_bindings_are_generated_from_this_wit() {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let wit = std::fs::read_to_string(here.join("wit/plugin.wit")).unwrap();
    let wit = wit.replace('\r', "");
    let digest: String = Sha256::digest(wit.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let bindings =
        std::fs::read_to_string(here.join("../clew-plugin-sdk/src/bindings.rs")).unwrap();
    let first = bindings.lines().next().unwrap_or_default();
    assert_eq!(
        first,
        format!("// clew: generated from crates/clew-plugin/wit/plugin.wit, sha256 {digest}"),
        "wit/plugin.wit changed: run crates/clew-plugin-sdk/generate.sh"
    );
}
