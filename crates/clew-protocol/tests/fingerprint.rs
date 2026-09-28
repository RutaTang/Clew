//! What the protocol fingerprint reacts to — code — and what it must not:
//! comments, doc comments, formatting and test modules. A fingerprint that
//! moved on a comment forced a remote redeploy for a byte-identical wire.
//!
//! And WHICH source it is taken of: the checkout being built, named when the
//! build script runs. A path fixed when the script was compiled named whatever
//! checkout compiled it — and a compiled script outlives its checkout in a
//! copied or shared target directory.

#[path = "../fingerprint.rs"]
mod fingerprint;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fingerprint::canonicalize;

const BASE: &str = r#"
/// A frame.
#[derive(Serialize)]
pub struct Frame {
    pub id: u64,
    #[serde(default)]
    pub name: String,
}
"#;

fn same(a: &str, b: &str) -> bool {
    canonicalize(a).unwrap() == canonicalize(b).unwrap()
}

/// The crate directory cargo runs this test for. Read at RUN time for the
/// reason `build.rs` reads it so: a test binary compiled in another checkout
/// would otherwise compare this build's constant against THAT checkout.
fn manifest_dir() -> PathBuf {
    PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo test sets CARGO_MANIFEST_DIR"),
    )
}

/// A scratch copy of a crate's layout, unique to this process and call, and
/// removed with everything in it when dropped.
struct Checkout(PathBuf);

impl Checkout {
    fn new(files: &[(&str, &str)]) -> Checkout {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clew-protocol-fingerprint-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, text) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        Checkout(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A checkout whose source is NOT this crate's: two files, one nested.
fn relocated() -> Checkout {
    Checkout::new(&[
        ("src/lib.rs", "pub mod wire;\npub const V: u32 = 1;\n"),
        ("src/wire/frame.rs", BASE),
        ("src/notes.txt", "not rust, not hashed"),
    ])
}

/// The paths `directives` asks cargo to watch.
fn watched(directives: &[String]) -> Vec<PathBuf> {
    directives
        .iter()
        .filter_map(|d| d.strip_prefix("cargo:rerun-if-changed="))
        .map(PathBuf::from)
        .collect()
}

#[test]
fn the_embedded_fingerprint_is_this_checkouts_source() {
    assert_eq!(
        fingerprint::of_crate(&manifest_dir()).unwrap().value,
        clew_protocol::SCHEMA_FINGERPRINT,
        "build.rs hashed another source than this checkout's — a stale path, or the source \
         changed after the build"
    );
}

/// Every file the fingerprint reads re-runs the build when it changes, and
/// nothing outside the checkout it was handed is read or watched.
#[test]
fn every_file_the_fingerprint_reads_is_watched_in_the_checkout_it_was_given() {
    let checkout = relocated();
    let fp = fingerprint::of_crate(checkout.path()).unwrap();
    assert_eq!(
        fp.files,
        [
            checkout.path().join("src/lib.rs"),
            checkout.path().join("src/wire/frame.rs")
        ],
        "every .rs file under src/, nothing else"
    );
    let directives = fingerprint::build_directives(checkout.path()).unwrap();
    let watched = watched(&directives);
    for file in &fp.files {
        assert!(
            watched.contains(file),
            "{} is read but not watched",
            file.display()
        );
    }
    assert!(
        watched.contains(&checkout.path().join("src")),
        "src/ itself is watched, so an added file re-runs the build"
    );
    for path in &watched {
        assert!(
            path.starts_with(checkout.path()),
            "{} is outside the checkout",
            path.display()
        );
    }
    assert_eq!(
        directives.last().unwrap(),
        &format!("cargo:rustc-env=CLEW_PROTOCOL_FINGERPRINT={}", fp.value)
    );
    assert_ne!(
        fp.value,
        clew_protocol::SCHEMA_FINGERPRINT,
        "the relocated source is not this crate's"
    );

    // An edit in the checkout moves ITS fingerprint.
    std::fs::write(
        checkout.path().join("src/wire/frame.rs"),
        BASE.replace("id: u64", "id: u32"),
    )
    .unwrap();
    assert_ne!(
        fingerprint::of_crate(checkout.path()).unwrap().value,
        fp.value
    );
}

/// The build script cargo compiled for this crate: the newest
/// `build-script-build` under the `build/` directory beside the `deps/` this
/// test binary runs from (`build/clew-protocol-<hash>/`). Newest, because a
/// target directory keeps one per configuration it has built, and the build
/// that produced this test compiled its own last.
fn compiled_build_script() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary's path");
    let build = exe
        .parent()
        .and_then(Path::parent)
        .expect("the test binary runs from <target>/<profile>/deps")
        .join("build");
    let name = format!("build-script-build{}", std::env::consts::EXE_SUFFIX);
    std::fs::read_dir(&build)
        .unwrap_or_else(|e| panic!("{}: {e}", build.display()))
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("clew-protocol-"))
        })
        .map(|entry| entry.path().join(&name))
        .filter_map(|script| Some((std::fs::metadata(&script).ok()?.modified().ok()?, script)))
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, script)| script)
        .unwrap_or_else(|| {
            panic!(
                "no compiled clew-protocol build script under {}",
                build.display()
            )
        })
}

/// The regression itself: the COMPILED script, run for a checkout other than
/// the one it was compiled in, must hash and watch that checkout. With the
/// directory taken by `env!` it answered for this crate instead, whatever
/// cargo told it at run time.
#[test]
fn the_build_script_hashes_the_checkout_it_is_run_for() {
    let checkout = relocated();
    let script = compiled_build_script();
    let out = std::process::Command::new(&script)
        .env_clear()
        .env("CARGO_MANIFEST_DIR", checkout.path())
        .output()
        .unwrap_or_else(|e| panic!("run {}: {e}", script.display()));
    assert!(
        out.status.success(),
        "{} failed: {}",
        script.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let printed: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        printed,
        fingerprint::build_directives(checkout.path()).unwrap(),
        "{} answered for another checkout than the one it was run for",
        script.display()
    );
}

#[test]
fn comments_and_formatting_do_not_count() {
    // A reworded doc comment, a new line comment, a block comment.
    assert!(same(
        BASE,
        r#"
/// A frame, reworded — and longer.
/// Two lines now.
#[derive(Serialize)]
pub struct Frame {
    // The id.
    pub id: u64, /* trailing */
    #[serde(default)]
    pub name: String,
}
"#
    ));
    // Reflowed onto one line.
    assert!(same(
        BASE,
        "#[derive(Serialize)] pub struct Frame { pub id: u64, #[serde(default)] pub name: String, }"
    ));
    // Inner doc comments too.
    assert!(same(&format!("//! Crate docs.\n{BASE}"), BASE));
}

#[test]
fn test_modules_do_not_count() {
    let with_tests = format!(
        "{BASE}\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn t() {{ assert!(true); }}\n}}\n"
    );
    assert!(same(&with_tests, BASE));
    // …but `cfg` in general is code.
    let with_cfg = format!("{BASE}\n#[cfg(unix)]\nconst X: u8 = 1;\n");
    assert!(!same(&with_cfg, BASE));
}

#[test]
fn code_changes_do_count() {
    // A field's type.
    assert!(!same(BASE, &BASE.replace("id: u64", "id: u32")));
    // A field's name.
    assert!(!same(BASE, &BASE.replace("pub name", "pub title")));
    // A serde attribute.
    assert!(!same(
        BASE,
        &BASE.replace("#[serde(default)]", "#[serde(rename = \"n\")]")
    ));
    // A string literal's content.
    let a = "const V: &str = \"a b\";";
    let b = "const V: &str = \"a  b\";";
    assert!(!same(a, b));
    // Operators stay apart: `::` is not `: :`.
    assert!(!same("type T = a::b;", "type T = a: :b;"));
}

#[test]
fn source_that_does_not_tokenize_is_an_error() {
    assert!(canonicalize("struct S { \"unterminated }").is_err());
}
