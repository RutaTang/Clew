//! The test plugins in `fixtures/`, built by Clew's own builder and checked
//! in, so that the runtime's tests need no WebAssembly target to run.
//!
//! `fixtures/<name>.wasm.sources` records the digest of everything a
//! fixture is built from (its source, the SDK, the WIT); a fixture whose
//! sources changed since is out of date. To build them again (Rust 1.95.0
//! with the wasm32-wasip2 target, as rust-toolchain.toml pins):
//!
//!     cargo test -p clew-plugin --test plugins -- --ignored

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use sha2::{Digest, Sha256};

const FIXTURES: [&str; 2] = ["views", "git-log"];

fn here() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every file under `dir`, by its path from `base`, sorted.
fn files(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            files(base, &path, out);
        } else {
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read(&path).unwrap();
            out.push((rel, text.into_iter().filter(|b| *b != b'\r').collect()));
        }
    }
}

/// The digest of what the fixture `name` is built from.
fn sources_digest(name: &str) -> String {
    let root = here();
    let mut all = Vec::new();
    let fixture = root.join("fixtures").join(name);
    files(&fixture, &fixture.join("src"), &mut all);
    for (rel, path) in [
        ("plugin.toml", fixture.join("plugin.toml")),
        ("sdk/Cargo.toml", root.join("../clew-plugin-sdk/Cargo.toml")),
        ("sdk/lib.rs", root.join("../clew-plugin-sdk/src/lib.rs")),
        ("sdk/rt.rs", root.join("../clew-plugin-sdk/src/rt.rs")),
        (
            "sdk/bindings.rs",
            root.join("../clew-plugin-sdk/src/bindings.rs"),
        ),
        ("wit", root.join("wit/plugin.wit")),
    ] {
        let text = std::fs::read(path).unwrap();
        all.push((
            rel.to_string(),
            text.into_iter().filter(|b| *b != b'\r').collect(),
        ));
    }
    let mut h = Sha256::new();
    for (rel, bytes) in &all {
        h.update((rel.len() as u64).to_le_bytes());
        h.update(rel.as_bytes());
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    clew_plugin::inspect::hex(&h.finalize())
}

#[test]
fn the_fixtures_are_built_from_their_sources() {
    for name in FIXTURES {
        let recorded =
            std::fs::read_to_string(here().join(format!("fixtures/{name}.wasm.sources")))
                .unwrap_or_default();
        assert_eq!(
            recorded.trim(),
            sources_digest(name),
            "fixtures/{name} changed since fixtures/{name}.wasm was built: build it again \
             (cargo test -p clew-plugin --test plugins -- --ignored)"
        );
        let bytes = std::fs::read(here().join(format!("fixtures/{name}.wasm"))).unwrap();
        clew_plugin::inspect(&bytes).unwrap();
    }
}

/// The toolchain to build the fixtures with: rustup's 1.95.0.
fn toolchain() -> clew_plugin::build::Toolchain {
    let which = |tool: &str| {
        let out = std::process::Command::new("rustup")
            .args(["which", "--toolchain", "1.95.0", tool])
            .output()
            .expect("rustup");
        assert!(out.status.success(), "rustup has no 1.95.0 toolchain");
        PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
    };
    clew_plugin::build::toolchain_from(which("cargo"), which("rustc"), true)
        .unwrap_or_else(|e| panic!("{}", e.report()))
}

#[test]
#[ignore = "builds the fixtures: needs Rust 1.95.0 with wasm32-wasip2, and the network"]
fn build_the_fixtures() {
    let toolchain = toolchain();
    let work = std::env::temp_dir().join("clew-plugin-fixture-builds");
    for name in FIXTURES {
        let source = here().join("fixtures").join(name);
        let built = clew_plugin::build::build(&source, &work, &toolchain, &AtomicBool::new(false))
            .unwrap_or_else(|e| panic!("{name}: {}", e.report()));
        assert!(built.warnings.is_empty(), "{name}: {:?}", built.warnings);
        std::fs::write(here().join(format!("fixtures/{name}.wasm")), &built.bytes).unwrap();
        std::fs::write(
            here().join(format!("fixtures/{name}.wasm.sources")),
            format!("{}\n", sources_digest(name)),
        )
        .unwrap();
        // The lock the build wrote back is not part of the fixture.
        let _ = std::fs::remove_file(source.join("Cargo.lock"));
    }
}
