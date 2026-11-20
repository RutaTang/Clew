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

/// Rust of Clew's own, installed for real into a data folder of its own,
/// builds a plugin. It downloads about 150 MB, so it runs only when asked:
///
///     CLEW_TEST_INSTALL_RUST=1 cargo test -p clew-plugin --test plugins \
///         installs_rust -- --ignored --nocapture
#[test]
#[ignore = "installs Rust from rust-lang.org: needs the network and CLEW_TEST_INSTALL_RUST=1"]
fn installs_rust_for_clew_and_builds_with_it() {
    use clew_plugin::toolchain::{Setup, Stage};
    if std::env::var_os("CLEW_TEST_INSTALL_RUST").is_none() {
        eprintln!("skipped: set CLEW_TEST_INSTALL_RUST=1 to install Rust for real");
        return;
    }
    let data = clew_core::testutil::TempDir::new("own-rust");
    let mut stages = Vec::new();
    let toolchain = clew_plugin::toolchain::set_up(
        &data,
        &Setup::Install,
        &AtomicBool::new(false),
        &mut |stage| stages.push(stage),
    )
    .unwrap_or_else(|why| panic!("{}", why.sentence()));
    assert_eq!(stages, [Stage::Downloading, Stage::Installing]);
    let dir = clew_plugin::toolchain::managed_dir(&data);
    assert!(
        !dir.join(format!("rustup-init{}", std::env::consts::EXE_SUFFIX))
            .exists(),
        "the installer is not kept"
    );
    // The user's Rust here may be ready; Clew's own, found as Clew finds it
    // when it is not, builds a plugin.
    let own = clew_plugin::toolchain::find(Some(&data)).unwrap();
    let own = match own.cargo_home {
        Some(_) => own,
        None => toolchain,
    };
    assert_eq!(own.cargo_home.as_deref(), Some(dir.join("cargo").as_path()));
    let work = data.join("work");
    let built = clew_plugin::build::build(
        &here().join("fixtures").join("views"),
        &work,
        &own,
        &AtomicBool::new(false),
    )
    .unwrap_or_else(|e| panic!("{}", e.report()));
    assert_eq!(built.inspected.manifest.plugin.id, "views");
    let size = std::process::Command::new("du")
        .args(["-sh"])
        .arg(&dir)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    eprintln!("Clew's own Rust {}: {}", own.version, size.trim());
}

/// A plugin built with crates agents reach for builds: the check that a
/// dependency reads only its own package does not refuse them. And one
/// whose own code reads a file of the computer's is refused before cargo
/// compiles anything. Downloads the crates, so it runs only when asked:
///
///     cargo test -p clew-plugin --test plugins popular -- --ignored --nocapture
#[test]
#[ignore = "downloads popular crates and builds with them: needs Rust 1.95.0 with wasm32-wasip2, and the network"]
fn popular_crates_build_and_reading_the_computer_does_not() {
    let toolchain = toolchain();
    let root = clew_core::testutil::TempDir::new("popular-crates");
    let source = root.join("source");
    std::fs::create_dir_all(source.join("src")).unwrap();
    std::fs::write(
        source.join("plugin.toml"),
        "[plugin]\nid = \"popular\"\nname = \"Popular\"\nversion = \"0.1.0\"\n\
         description = \"Uses popular crates.\"\n\n[[command]]\nid = \"run\"\ntitle = \"Run\"\n\n\
         [dependencies]\nregex = \"1\"\nserde = \"1\"\nserde_json = \"1\"\nitertools = \"0.14\"\n\
         unicode-segmentation = \"1\"\nchrono = \"0.4\"\nurl = \"2\"\nindexmap = \"2\"\n\
         smallvec = \"1\"\nsemver = \"1\"\nglob = \"0.3\"\nsimilar = \"2\"\ntoml = \"0.8\"\n\
         heck = \"0.5\"\nstrsim = \"0.11\"\nonce_cell = \"1\"\nzerocopy = \"0.8\"\n",
    )
    .unwrap();
    std::fs::write(
        source.join("src/lib.rs"),
        r#"use clew_plugin_sdk::{Invocation, Outcome, Plugin};
struct P;
impl Plugin for P {
    fn run(_command: &str, _context: Invocation) -> Result<Outcome, String> {
        let re = regex::Regex::new(r"\d+").map_err(|e| e.to_string())?;
        let url = url::Url::parse("https://example.com/a?b=1").map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_str("{\"a\":1}").map_err(|e| e.to_string())?;
        let t: toml::Table = "a = 1".parse().map_err(|e: toml::de::Error| e.to_string())?;
        let words = unicode_segmentation::UnicodeSegmentation::unicode_words("a b").count();
        let joined = itertools::Itertools::join(&mut ["a", "b"].iter(), ",");
        let mut map = indexmap::IndexMap::new();
        map.insert(heck::ToSnakeCase::to_snake_case("AB"), strsim::levenshtein("a", "b"));
        let small: smallvec::SmallVec<[u8; 4]> = smallvec::smallvec![1];
        let version = semver::Version::parse("1.2.3").map_err(|e| e.to_string())?;
        let pattern = glob::Pattern::new("*.rs").map_err(|e| e.to_string())?;
        let diff = similar::TextDiff::from_lines("a\n", "b\n").ratio();
        let day = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).map(|d| d.to_string());
        static ONCE: once_cell::sync::Lazy<u8> = once_cell::sync::Lazy::new(|| 1);
        // zerocopy's `include_value!` reads a path from wherever it is
        // used: what it reads is the compiler's to say, not a refusal.
        let bytes = zerocopy::IntoBytes::as_bytes(&7u32).len();
        Ok(Outcome::Say(format!(
            "{} {url} {v} {t} {words} {joined} {map:?} {small:?} {version} {} {diff} {day:?} {} {bytes}",
            re.is_match("1"),
            pattern.matches("a.rs"),
            *ONCE
        )))
    }
}
clew_plugin_sdk::export!(P);
"#,
    )
    .unwrap();
    let work = root.join("work");
    let built = clew_plugin::build::build(&source, &work, &toolchain, &AtomicBool::new(false))
        .unwrap_or_else(|e| panic!("{}", e.report()));
    assert!(built.warnings.is_empty(), "{:?}", built.warnings);

    // Its own code reading a file: refused, saying where and why.
    std::fs::write(
        source.join("src/lib.rs"),
        "use clew_plugin_sdk::{Invocation, Outcome, Plugin};\nstruct P;\n\
         impl Plugin for P {\n    fn run(_c: &str, _i: Invocation) -> Result<Outcome, String> {\n\
         Ok(Outcome::Say(include_str!(\"../../../config.toml\").into()))\n    }\n}\n\
         clew_plugin_sdk::export!(P);\n",
    )
    .unwrap();
    let refused = clew_plugin::build::build(&source, &work, &toolchain, &AtomicBool::new(false))
        .expect_err("refused");
    assert!(
        refused
            .report()
            .contains("src/lib.rs:5: `include_str` reads a file"),
        "{}",
        refused.report()
    );
}

/// What a plugin's macro puts together to read a file of the computer's —
/// an attribute made from its input, which no reading of the source can
/// see whole — is caught by what the compiler says it read: refused,
/// nothing the compiler said passed on (here, the size a type error would
/// have told), and what was compiled from it gone. Whether the build then
/// failed, or would have succeeded, is the same.
///
///     cargo test -p clew-plugin --test plugins macro_built -- --ignored --nocapture
#[test]
#[ignore = "builds plugins: needs Rust 1.95.0 with wasm32-wasip2, and the network"]
fn macro_built_reads_of_the_computer_are_refused() {
    let toolchain = toolchain();
    let root = clew_core::testutil::TempDir::new("macro-built-reads");
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.rs"), "pub const X: usize = 5;\n").unwrap();
    std::fs::write(outside.join("secret.txt"), "machine x password y\n").unwrap();
    let source = root.join("source");
    std::fs::create_dir_all(source.join("src")).unwrap();
    std::fs::write(
        source.join("plugin.toml"),
        "[plugin]\nid = \"reads\"\nname = \"Reads\"\nversion = \"0.1.0\"\n\
         description = \"Reads what it may not.\"\n\n[[command]]\nid = \"run\"\ntitle = \"Run\"\n",
    )
    .unwrap();
    let work = root.join("work");
    let plugin = |body: &str| {
        format!(
            "use clew_plugin_sdk::{{Invocation, Outcome, Plugin}};\n\
             macro_rules! load {{ ($a:meta) => {{ #[$a] mod leak; }} }}\n{body}\nstruct P;\n\
             impl Plugin for P {{\n    fn run(_c: &str, _i: Invocation) -> Result<Outcome, String> {{\n\
             Ok(Outcome::Say(String::new()))\n    }}\n}}\nclew_plugin_sdk::export!(P);\n"
        )
    };
    let secret_rs = outside.join("secret.rs").display().to_string();
    let secret_txt = outside.join("secret.txt").display().to_string();
    for (body, read, not_told) in [
        // Read, and its value told by a type error.
        (
            format!("load!(path = {secret_rs:?});\nconst _: [(); 0] = [(); leak::X];"),
            &secret_rs,
            "size of 5",
        ),
        // Read as Rust, and its words told by the parse error.
        (
            format!("load!(path = {secret_txt:?});"),
            &secret_txt,
            "password",
        ),
        // Read, and compiled into the plugin.
        (
            format!("load!(path = {secret_rs:?});\npub fn x() -> usize {{ leak::X }}"),
            &secret_rs,
            "leak::X",
        ),
    ] {
        std::fs::write(source.join("src/lib.rs"), plugin(&body)).unwrap();
        let refused =
            clew_plugin::build::build(&source, &work, &toolchain, &AtomicBool::new(false))
                .expect_err("refused");
        let report = refused.report();
        assert!(
            matches!(&refused, clew_plugin::build::BuildError::Policy(why) if why.contains(read.as_str())),
            "{body}: {report}"
        );
        assert!(!report.contains(not_told), "{body}: {report}");
        assert!(
            work.join("targets").is_dir() && !work.join("targets/reads").exists(),
            "{body}: what was compiled is kept"
        );
    }
}

/// What the compiler said comes back when it said what it read: a type
/// error, built once and built again unchanged (its dep-info file written
/// again word for word). A bracket left open stops it before it says what
/// it read: Clew says where, in its own words.
///
///     cargo test -p clew-plugin --test plugins compilers_words -- --ignored --nocapture
#[test]
#[ignore = "builds plugins: needs Rust 1.95.0 with wasm32-wasip2, and the network"]
fn the_compilers_words_are_passed_on_when_it_said_what_it_read() {
    let toolchain = toolchain();
    let root = clew_core::testutil::TempDir::new("compilers-words");
    let source = root.join("source");
    std::fs::create_dir_all(source.join("src")).unwrap();
    std::fs::write(
        source.join("plugin.toml"),
        "[plugin]\nid = \"words\"\nname = \"Words\"\nversion = \"0.1.0\"\n\
         description = \"Does not compile.\"\n\n[[command]]\nid = \"run\"\ntitle = \"Run\"\n",
    )
    .unwrap();
    let work = root.join("work");
    let plugin = |body: &str| {
        format!(
            "use clew_plugin_sdk::{{Invocation, Outcome, Plugin}};\n{body}\nstruct P;\n\
             impl Plugin for P {{\n    fn run(_c: &str, _i: Invocation) -> Result<Outcome, String> {{\n\
             Ok(Outcome::Say(String::new()))\n    }}\n}}\nclew_plugin_sdk::export!(P);\n"
        )
    };
    let build = || clew_plugin::build::build(&source, &work, &toolchain, &AtomicBool::new(false));
    std::fs::write(
        source.join("src/lib.rs"),
        plugin("pub fn f() -> u32 {\n    \"x\"\n}"),
    )
    .unwrap();
    for _ in 0..2 {
        let failed = build().expect_err("a type error");
        assert!(
            matches!(&failed, clew_plugin::build::BuildError::Compile(errors)
                if errors.iter().any(|e| e.contains("E0308"))),
            "{}",
            failed.report()
        );
    }
    std::fs::write(
        source.join("src/lib.rs"),
        plugin("pub fn f() -> u32 {\n    (1\n}"),
    )
    .unwrap();
    let failed = build().expect_err("a bracket left open");
    assert!(
        matches!(&failed, clew_plugin::build::BuildError::Compile(errors)
            if errors.len() == 1
                && errors[0].starts_with("error: src/lib.rs:4: this `}` closes the `(` of line 3")),
        "{}",
        failed.report()
    );
    // A statement where an item goes stops it too: Clew says what does not
    // parse there, as it reads the file.
    std::fs::write(source.join("src/lib.rs"), plugin("let x = 1;")).unwrap();
    let failed = build().expect_err("a statement where an item goes");
    assert!(
        matches!(&failed, clew_plugin::build::BuildError::Compile(errors)
            if errors.len() == 1 && errors[0].starts_with("error: src/lib.rs:2:1: expected")),
        "{}",
        failed.report()
    );
}
