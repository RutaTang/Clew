//! Embed a fingerprint of the protocol source into both binaries.
//!
//! The numeric `PROTOCOL_VERSION` is bumped by hand and can lag a wire change
//! (two builds both claiming the same version but serializing different
//! shapes would handshake fine and then silently drop each other's frames).
//! The fingerprint closes that gap mechanically: it hashes the protocol
//! source itself, so ANY change — even one a human forgot to version — makes
//! the two sides refuse each other at Hello/Ready and triggers a redeploy.
//!
//! FNV-1a, not a cryptographic hash: this guards against accidental drift
//! between two of our own builds, not against an attacker (who controls the
//! binary and thus the constant anyway).

use std::path::Path;

fn main() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("lib.rs");
    println!("cargo:rerun-if-changed={}", src.display());
    let bytes = std::fs::read(&src).expect("read clew-protocol source for fingerprinting");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    println!("cargo:rustc-env=CLEW_PROTOCOL_FINGERPRINT={hash:016x}");
}
