//! Embed a fingerprint of the protocol source into both binaries.
//!
//! The numeric `PROTOCOL_VERSION` is bumped by hand and can lag a wire change
//! (two builds both claiming the same version but serializing different
//! shapes would handshake fine and then silently drop each other's frames).
//! The fingerprint closes that gap mechanically: it hashes the protocol CODE —
//! every `.rs` file under `src/`, the payload types included — so any change a
//! human forgot to version makes the two sides refuse each other at
//! Hello/Ready and triggers a redeploy. Comments, formatting and test modules
//! are not code on the wire and do not count (see `fingerprint.rs`).

mod fingerprint;

use std::path::Path;

fn main() {
    // The checkout to hash is read when the script RUNS, never with `env!`,
    // which bakes in the directory the script was COMPILED in. A compiled
    // build script is reused from whatever target directory holds it — a
    // `target/` copied into a fresh worktree, or one shared by several
    // checkouts — and cargo re-runs it without recompiling when only `src/`
    // changed. A compile-time path then names the OTHER checkout: the build
    // hashed that source, stamped this binary with its fingerprint, and watched
    // its files for changes. `tests/fingerprint.rs` runs the compiled script
    // against a relocated checkout to keep it that way.
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("cargo sets CARGO_MANIFEST_DIR for every build script it runs");
    let directives = fingerprint::build_directives(Path::new(&manifest_dir))
        .unwrap_or_else(|e| panic!("fingerprinting the clew-protocol source failed: {e}"));
    for line in directives {
        println!("{line}");
    }
}
