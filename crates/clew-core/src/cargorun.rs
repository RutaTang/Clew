//! Running a Rust program, or one of its tests, the way Cargo builds it:
//! which target of which package a file is compiled in, and its module
//! there; the `cargo` command that builds that target; the executable the
//! build made, read from Cargo's own report of it; and the name a test goes
//! by in its test binary.
//!
//! Run and Debug build with the user's own Cargo, as a terminal would, and
//! never from a launch file: what runs is the program the reader is
//! reading, built as `cargo run` would build it.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The kind of Cargo target a file is compiled in, with its name where
/// Cargo selects it by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    Lib,
    Bin(String),
    Example(String),
    Test(String),
    Bench(String),
}

/// A file's place in a Cargo package: the target it is compiled in, and the
/// module it is in that target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoTarget {
    /// The package's `Cargo.toml`.
    pub manifest: PathBuf,
    /// The package's name.
    pub package: String,
    pub kind: TargetKind,
    /// The file's module within the target, from its root (`parser::lexer`);
    /// empty for the target's root file.
    pub module: Vec<String>,
}

impl CargoTarget {
    /// The package's folder.
    pub fn package_dir(&self) -> &Path {
        self.manifest.parent().unwrap_or(Path::new("."))
    }

    /// Cargo's flags selecting this target alone.
    pub fn select(&self) -> Vec<String> {
        match &self.kind {
            TargetKind::Lib => vec!["--lib".into()],
            TargetKind::Bin(n) => vec!["--bin".into(), n.clone()],
            TargetKind::Example(n) => vec!["--example".into(), n.clone()],
            TargetKind::Test(n) => vec!["--test".into(), n.clone()],
            TargetKind::Bench(n) => vec!["--bench".into(), n.clone()],
        }
    }

    /// The target as the reader would name it: "the library", "binary
    /// `app`", "test `api`".
    pub fn describe(&self) -> String {
        match &self.kind {
            TargetKind::Lib => format!("{} (library)", self.package),
            TargetKind::Bin(n) => n.clone(),
            TargetKind::Example(n) => format!("example {n}"),
            TargetKind::Test(n) => format!("test {n}"),
            TargetKind::Bench(n) => format!("bench {n}"),
        }
    }

    /// Whether this target is a program Cargo runs: a binary or an example.
    pub fn runs(&self) -> bool {
        matches!(self.kind, TargetKind::Bin(_) | TargetKind::Example(_))
    }

    /// The name of the test `name`, written in the inline modules `inline`
    /// (outermost first) of this target's file, as its test binary lists it:
    /// `parser::tests::adds`.
    pub fn test_path(&self, inline: &[String], name: &str) -> String {
        self.module
            .iter()
            .chain(inline)
            .map(String::as_str)
            .chain(std::iter::once(name))
            .collect::<Vec<_>>()
            .join("::")
    }
}

/// What Cargo is told of features: the features the reader chose for the
/// project (Settings → Language servers), so the program runs as the code
/// reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Features {
    pub all: bool,
    pub list: Vec<String>,
    pub no_default: bool,
}

impl Features {
    pub fn from_choices(choices: Option<&clew_protocol::RustChoices>) -> Features {
        match choices {
            Some(c) => Features {
                all: c.all_features,
                list: c.features.clone(),
                no_default: c.no_default_features,
            },
            None => Features::default(),
        }
    }

    fn args(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.all {
            out.push("--all-features".into());
        } else if !self.list.is_empty() {
            out.push("--features".into());
            out.push(self.list.join(","));
        }
        if self.no_default {
            out.push("--no-default-features".into());
        }
        out
    }
}

/// The most of a manifest or a crate root read: a repository's `Cargo.toml`
/// or `lib.rs` that is a FIFO, or a link to `/dev/zero`, hung or filled the
/// app reading it.
const MAX_READ_BYTES: u64 = 1024 * 1024;

/// The package a file is in — the nearest `Cargo.toml` above it with a
/// `[package]` — and its manifest.
fn package_of(file: &Path) -> Option<(PathBuf, toml::Value)> {
    let mut dir = file.parent();
    while let Some(d) = dir {
        let manifest = d.join("Cargo.toml");
        if let Some(text) = crate::statefile::read_capped(&manifest, MAX_READ_BYTES)
            && let Ok(value) = toml::from_str::<toml::Value>(&text)
            && value.get("package").is_some()
        {
            return Some((manifest, value));
        }
        dir = d.parent();
    }
    None
}

/// The file's path below `dir`, its parts.
fn parts_below(file: &Path, dir: &Path) -> Option<Vec<String>> {
    let rel = file.strip_prefix(dir).ok()?;
    rel.components()
        .map(|c| c.as_os_str().to_str().map(str::to_string))
        .collect()
}

/// The module a file at `parts` (below its target's root folder, the root
/// file itself included as the first) is: `a/b.rs` is `a::b`, `a/mod.rs`
/// is `a`.
fn module_of(parts: &[String]) -> Vec<String> {
    let mut module: Vec<String> = parts.to_vec();
    if let Some(last) = module.pop() {
        let stem = last.strip_suffix(".rs").unwrap_or(&last);
        if stem != "mod" {
            module.push(stem.to_string());
        }
    }
    module
}

/// The target `file` is compiled in, and its module there, as Cargo finds
/// targets: the paths a manifest names (`[lib] path`, `[[bin]] path`, …),
/// then its own layout — `src/lib.rs`, `src/main.rs`, `src/bin/`,
/// `examples/`, `tests/`, `benches/`. A module of `src/` is the library's
/// when the package has one — unless only `src/main.rs` declares it — else
/// the default binary's. `None` for a file
/// in no package, or in one Cargo would not build it in.
pub fn target_of(file: &Path) -> Option<CargoTarget> {
    let (manifest, value) = package_of(file)?;
    let dir = manifest.parent()?.to_path_buf();
    let package = value.get("package")?.get("name")?.as_str()?.to_string();
    let parts = parts_below(file, &dir)?;
    let target = |kind: TargetKind, module: Vec<String>| {
        Some(CargoTarget {
            manifest: manifest.clone(),
            package: package.clone(),
            kind,
            module,
        })
    };
    // Named in the manifest, by path: that target, its root.
    let declared = |table: &str| -> Vec<(Option<String>, PathBuf)> {
        let items: Vec<&toml::Value> = match value.get(table) {
            Some(toml::Value::Array(a)) => a.iter().collect(),
            Some(t @ toml::Value::Table(_)) => vec![t],
            _ => Vec::new(),
        };
        items
            .into_iter()
            .filter_map(|t| {
                let path = t.get("path")?.as_str()?;
                Some((
                    t.get("name").and_then(|n| n.as_str()).map(str::to_string),
                    dir.join(path),
                ))
            })
            .collect()
    };
    for (table, kind) in [
        ("lib", None),
        ("bin", Some(TargetKind::Bin as fn(String) -> TargetKind)),
        (
            "example",
            Some(TargetKind::Example as fn(String) -> TargetKind),
        ),
        ("test", Some(TargetKind::Test as fn(String) -> TargetKind)),
        ("bench", Some(TargetKind::Bench as fn(String) -> TargetKind)),
    ] {
        for (name, path) in declared(table) {
            if path != file {
                continue;
            }
            let kind = match kind {
                None => TargetKind::Lib,
                Some(make) => make(name.unwrap_or_else(|| stem(file))),
            };
            return target(kind, Vec::new());
        }
    }
    let first = parts.first()?.as_str();
    match first {
        "src" => {
            let rest = &parts[1..];
            match rest {
                [f] if f == "lib.rs" => target(TargetKind::Lib, Vec::new()),
                [f] if f == "main.rs" => {
                    target(TargetKind::Bin(default_bin(&value, &package)), Vec::new())
                }
                [bin, f] if bin == "bin" && f.ends_with(".rs") => {
                    target(TargetKind::Bin(stem_of(f)), Vec::new())
                }
                [bin, name, rest @ ..] if bin == "bin" && !rest.is_empty() => {
                    let module = match rest {
                        [f] if f == "main.rs" => Vec::new(),
                        _ => module_of(rest),
                    };
                    target(TargetKind::Bin(name.clone()), module)
                }
                _ => {
                    let lib_root = value
                        .get("lib")
                        .and_then(|lib| lib.get("path")?.as_str())
                        .map_or_else(|| dir.join("src/lib.rs"), |path| dir.join(path));
                    let main = dir.join("src/main.rs");
                    let has_lib = lib_root.is_file() || value.get("lib").is_some();
                    // A package with both: the module is the program's when
                    // its root declares it and the library's does not. Taken
                    // for the library's, a test in `src/cli.rs` (`mod cli;`
                    // in `main.rs`) was looked for in the library, and the
                    // library's own test of the same name ran in its place.
                    let the_programs = |top: &str| {
                        main.is_file() && !declares_mod(&lib_root, top) && declares_mod(&main, top)
                    };
                    let module = module_of(rest);
                    let kind = match module.first() {
                        Some(top) if has_lib && the_programs(top) => {
                            TargetKind::Bin(default_bin(&value, &package))
                        }
                        _ if has_lib => TargetKind::Lib,
                        _ if main.is_file() => TargetKind::Bin(default_bin(&value, &package)),
                        _ => return None,
                    };
                    target(kind, module)
                }
            }
        }
        "examples" | "tests" | "benches" => {
            let make = match first {
                "examples" => TargetKind::Example,
                "tests" => TargetKind::Test,
                _ => TargetKind::Bench,
            };
            match &parts[1..] {
                [f] if f.ends_with(".rs") => target(make(stem_of(f)), Vec::new()),
                // `tests/api/main.rs` is the target `api`; its other files
                // are its modules. (`tests/common/mod.rs` is no target: a
                // module each test that wants it declares.)
                [name, rest @ ..] if dir.join(first).join(name).join("main.rs").is_file() => {
                    let module = match rest {
                        [f] if f == "main.rs" => Vec::new(),
                        _ => module_of(rest),
                    };
                    target(make(name.clone()), module)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether the crate root at `root` declares the module `name` — `mod
/// name;` or `mod name { … }`, public or not — on a line of its own.
fn declares_mod(root: &Path, name: &str) -> bool {
    let Some(text) = crate::statefile::read_capped(root, MAX_READ_BYTES) else {
        return false;
    };
    text.lines().any(|line| {
        let mut line = line.trim_start();
        if let Some(rest) = line.strip_prefix("pub") {
            line = rest.trim_start();
            if let Some(scoped) = line.strip_prefix('(') {
                let Some((_, after)) = scoped.split_once(')') else {
                    return false;
                };
                line = after.trim_start();
            }
        }
        line.strip_prefix("mod")
            .filter(|rest| rest.starts_with(char::is_whitespace))
            .and_then(|rest| rest.trim_start().strip_prefix(name))
            .is_some_and(|rest| matches!(rest.trim_start().chars().next(), Some(';' | '{')))
    })
}

/// The default binary's name: the `[[bin]]` at `src/main.rs`'s, else the
/// package's.
fn default_bin(value: &toml::Value, package: &str) -> String {
    value
        .get("bin")
        .and_then(|b| b.as_array())
        .and_then(|bins| {
            bins.iter().find_map(|b| {
                let path = b.get("path").and_then(|p| p.as_str());
                (path.is_none() || path == Some("src/main.rs"))
                    .then(|| b.get("name")?.as_str().map(str::to_string))
                    .flatten()
            })
        })
        .unwrap_or_else(|| package.to_string())
}

fn stem(file: &Path) -> String {
    file.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

fn stem_of(name: &str) -> String {
    name.strip_suffix(".rs").unwrap_or(name).to_string()
}

/// Cargo's arguments to build `target` to run it (`build`), or its tests
/// to run one (`test --no-run`), reporting what it built as JSON on its
/// standard output and its messages, as the reader would read them, on its
/// standard error.
pub fn build_args(target: &CargoTarget, test: bool, features: &Features) -> Vec<String> {
    let mut args: Vec<String> = match test {
        true => vec!["test".into(), "--no-run".into()],
        false => vec!["build".into()],
    };
    args.extend([
        "--manifest-path".into(),
        target.manifest.to_string_lossy().into_owned(),
        "--message-format=json-render-diagnostics".into(),
        "--color".into(),
        "never".into(),
    ]);
    args.extend(target.select());
    args.extend(features.args());
    args
}

/// The executable Cargo reports, in its JSON messages `stdout`, having
/// built for `target`: the program, or (`test`) its test binary.
pub fn built_executable(stdout: &str, target: &CargoTarget, test: bool) -> Option<PathBuf> {
    let package_dir = target.package_dir();
    stdout.lines().find_map(|line| {
        let message: serde_json::Value = serde_json::from_str(line).ok()?;
        if message["reason"] != "compiler-artifact" {
            return None;
        }
        let executable = message["executable"].as_str()?;
        if message["profile"]["test"].as_bool() != Some(test) {
            return None;
        }
        let src = Path::new(message["target"]["src_path"].as_str()?);
        if !src.starts_with(package_dir) {
            return None;
        }
        let kinds: Vec<&str> = message["target"]["kind"]
            .as_array()?
            .iter()
            .filter_map(|k| k.as_str())
            .collect();
        let name = message["target"]["name"].as_str()?;
        let matches = match &target.kind {
            TargetKind::Lib => kinds.iter().any(|k| {
                matches!(
                    *k,
                    "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
                )
            }),
            TargetKind::Bin(n) => kinds.contains(&"bin") && name == n,
            TargetKind::Example(n) => kinds.contains(&"example") && name == n,
            TargetKind::Test(n) => kinds.contains(&"test") && name == n,
            TargetKind::Bench(n) => kinds.contains(&"bench") && name == n,
        };
        matches.then(|| PathBuf::from(executable))
    })
}

/// Of a test binary's `--list --format terse` lines (`a::tests::adds:
/// test`), the test the reader meant: `wanted`, its full name as Clew
/// worked it out, when listed; else the one test whose name ends in all of
/// it, below modules Clew did not see (a `#[path]`'d module). Never one
/// that only ends in the test's own name: `cli::tests::parses` not listed,
/// the library's `tests::parses` ran in its place, and passed for it.
pub fn listed_test(list: &str, wanted: &str) -> Option<String> {
    let tests: Vec<&str> = list
        .lines()
        .filter_map(|l| l.strip_suffix(": test"))
        .collect();
    if tests.contains(&wanted) {
        return Some(wanted.to_string());
    }
    let suffix = format!("::{wanted}");
    let mut ending = tests.iter().filter(|t| t.ends_with(&suffix));
    match (ending.next(), ending.next()) {
        (Some(one), None) => Some(one.to_string()),
        _ => None,
    }
}

/// The user's Cargo, on the path a launched program gets — where rustup
/// puts it (`~/.cargo/bin`) included, which an app started from the desktop
/// often has not on its own ([`crate::linerpc::spawn_path`]).
pub fn find_cargo() -> Option<PathBuf> {
    crate::lsp::store::find_in_unresolved(&crate::linerpc::child_path(), "cargo")
}

/// How long a test binary may take to list its tests.
const LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// Why a build gave nothing to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// Cargo could not be started; the system's word why.
    Spawn(String),
    /// Stopped before it was done ([`build`]'s `cancel`).
    Stopped,
    /// The build failed: Cargo said why, in the lines it wrote.
    Failed,
    /// Cargo built, and reported no program for the target.
    NoProgram,
}

/// Build `target` with `cargo`, each line Cargo writes for the reader
/// (`Compiling …`, an error as rustc renders it) handed to `line` as it
/// comes; the executable it built. Blocking; given up, the build killed,
/// once `cancel` is set — checked as each line comes and, while Cargo is
/// silent (linking), every few tenths of a second.
pub fn build(
    cargo: &Path,
    target: &CargoTarget,
    test: bool,
    features: &Features,
    mut line: impl FnMut(&str),
    cancel: &AtomicBool,
) -> Result<PathBuf, BuildError> {
    let mut command = crate::spawn::command(cargo);
    command
        .args(build_args(target, test, features))
        .current_dir(target.package_dir())
        .env("PATH", crate::linerpc::child_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| BuildError::Spawn(e.to_string()))?;
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        if let Some(mut stdout) = stdout {
            let _ = stdout.read_to_string(&mut out);
        }
        out
    });
    // Cargo's messages to the reader, read on a thread of their own: read
    // here, a silent link step held the stop until it wrote again.
    let (lines_tx, lines_rx) = std::sync::mpsc::channel::<String>();
    let stderr = child.stderr.take();
    let messages = std::thread::spawn(move || {
        if let Some(stderr) = stderr {
            for l in BufReader::new(stderr).lines().map_while(Result::ok) {
                if lines_tx.send(l).is_err() {
                    break;
                }
            }
        }
    });
    let status = loop {
        while let Ok(l) = lines_rx.try_recv() {
            line(&l);
        }
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(BuildError::Stopped);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if let Ok(l) = lines_rx.recv_timeout(Duration::from_millis(150)) {
                    line(&l);
                }
            }
            Err(e) => return Err(BuildError::Spawn(e.to_string())),
        }
    };
    let _ = messages.join();
    while let Ok(l) = lines_rx.try_recv() {
        line(&l);
    }
    let out = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(BuildError::Failed);
    }
    built_executable(&out, target, test).ok_or(BuildError::NoProgram)
}

/// The tests a test binary has, as `--list --format terse` lists them.
pub fn list_tests(executable: &Path, cwd: &Path) -> Option<String> {
    let mut child = crate::spawn::command(executable)
        .args(["--list", "--format", "terse"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });
    let deadline = std::time::Instant::now() + LIST_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
}

/// A launch body (lldb-dap's) with what Cargo tells a program it runs, that
/// the program may read as it runs: its package's folder,
/// `CARGO_MANIFEST_DIR` — where snapshot tests and fixtures are found by.
/// Added to the variables the body sets, in the `VAR=VALUE` form lldb-dap
/// takes.
pub fn with_cargo_env(launch: &mut serde_json::Value, package_dir: &Path) {
    let Some(body) = launch.as_object_mut() else {
        return;
    };
    let entry = serde_json::Value::String(format!(
        "CARGO_MANIFEST_DIR={}",
        package_dir.to_string_lossy()
    ));
    match body.get_mut("env").and_then(|env| env.as_array_mut()) {
        Some(env) => env.push(entry),
        None => {
            body.insert("env".into(), serde_json::Value::Array(vec![entry]));
        }
    }
}

/// The arguments a test binary runs the one test `path` with: by its exact
/// name, an ignored one too (asked for by name, it is meant), and its
/// output as it comes.
pub fn test_args(path: &str) -> Vec<String> {
    vec![
        path.to_string(),
        "--exact".into(),
        "--include-ignored".into(),
        "--nocapture".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A file is placed in the target Cargo builds it in, by the manifest's
    /// paths and then Cargo's own layout, with its module there.
    #[test]
    fn a_file_is_in_the_target_cargo_builds_it_in() {
        let dir = TempDir::new("cargorun-layout");
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"demo\"\n\n[[bin]]\nname = \"renamed\"\npath = \"tools/x.rs\"\n",
        );
        for rel in [
            "src/lib.rs",
            "src/main.rs",
            "src/parser/mod.rs",
            "src/parser/lexer.rs",
            "src/bin/tool.rs",
            "src/bin/big/main.rs",
            "src/bin/big/util.rs",
            "examples/ex.rs",
            "tests/api.rs",
            "tests/suite/main.rs",
            "tests/suite/helpers.rs",
            "tests/common/mod.rs",
            "benches/b.rs",
            "tools/x.rs",
        ] {
            write(root, rel, "");
        }
        let at = |rel: &str| target_of(&root.join(rel)).map(|t| (t.kind, t.module.join("::")));
        let bin = |n: &str| TargetKind::Bin(n.into());
        assert_eq!(at("src/lib.rs"), Some((TargetKind::Lib, String::new())));
        assert_eq!(at("src/main.rs"), Some((bin("demo"), String::new())));
        assert_eq!(
            at("src/parser/mod.rs"),
            Some((TargetKind::Lib, "parser".into()))
        );
        assert_eq!(
            at("src/parser/lexer.rs"),
            Some((TargetKind::Lib, "parser::lexer".into()))
        );
        assert_eq!(at("src/bin/tool.rs"), Some((bin("tool"), String::new())));
        assert_eq!(at("src/bin/big/main.rs"), Some((bin("big"), String::new())));
        assert_eq!(at("src/bin/big/util.rs"), Some((bin("big"), "util".into())));
        assert_eq!(
            at("examples/ex.rs"),
            Some((TargetKind::Example("ex".into()), String::new()))
        );
        assert_eq!(
            at("tests/api.rs"),
            Some((TargetKind::Test("api".into()), String::new()))
        );
        assert_eq!(
            at("tests/suite/helpers.rs"),
            Some((TargetKind::Test("suite".into()), "helpers".into()))
        );
        assert_eq!(at("tests/common/mod.rs"), None, "a module tests share");
        assert_eq!(
            at("benches/b.rs"),
            Some((TargetKind::Bench("b".into()), String::new()))
        );
        assert_eq!(at("tools/x.rs"), Some((bin("renamed"), String::new())));
        // A module only the program's root declares is the program's.
        write(root, "src/main.rs", "mod cli;\n\nfn main() {}\n");
        write(root, "src/lib.rs", "pub mod parser;\n");
        write(root, "src/cli.rs", "");
        assert_eq!(at("src/cli.rs"), Some((bin("demo"), "cli".into())));
        assert_eq!(
            at("src/parser/lexer.rs"),
            Some((TargetKind::Lib, "parser::lexer".into()))
        );
        // Both declare it (a module the program compiles again): the
        // library's, as before.
        write(
            root,
            "src/lib.rs",
            "pub mod parser;\npub(crate) mod cli {}\n",
        );
        assert_eq!(at("src/cli.rs"), Some((TargetKind::Lib, "cli".into())));
        assert!(declares_mod(&root.join("src/lib.rs"), "cli"));
        assert!(!declares_mod(&root.join("src/lib.rs"), "cl"));
        assert!(!declares_mod(&root.join("src/main.rs"), "parser"));
        // No library: a module of `src/` is the program's.
        std::fs::remove_file(root.join("src/lib.rs")).unwrap();
        assert_eq!(
            at("src/parser/lexer.rs"),
            Some((bin("demo"), "parser::lexer".into()))
        );
        // Outside any package: none.
        let elsewhere = TempDir::new("cargorun-none");
        assert_eq!(target_of(&write(elsewhere.path(), "a.rs", "")), None);
    }

    /// Cargo is asked to build exactly the target, with the reader's
    /// features, and to say what it built as JSON; the test's name is its
    /// path through the target's modules.
    #[test]
    fn cargo_builds_the_target_with_the_features_chosen() {
        let target = CargoTarget {
            manifest: PathBuf::from("/p/Cargo.toml"),
            package: "demo".into(),
            kind: TargetKind::Test("api".into()),
            module: vec!["helpers".into()],
        };
        let features = Features {
            all: false,
            list: vec!["fast".into(), "demo/serde".into()],
            no_default: true,
        };
        assert_eq!(
            build_args(&target, true, &features),
            [
                "test",
                "--no-run",
                "--manifest-path",
                "/p/Cargo.toml",
                "--message-format=json-render-diagnostics",
                "--color",
                "never",
                "--test",
                "api",
                "--features",
                "fast,demo/serde",
                "--no-default-features",
            ]
        );
        assert_eq!(
            target.test_path(&["tests".into()], "adds"),
            "helpers::tests::adds"
        );
        assert_eq!(
            Features::from_choices(Some(&clew_protocol::RustChoices {
                all_features: true,
                ..Default::default()
            }))
            .args(),
            ["--all-features"]
        );
    }

    /// The program Cargo built is read from its own report: the artifact of
    /// this package's target, a test build or not as asked.
    #[test]
    fn the_program_built_is_read_from_cargos_report() {
        let artifact = |kind: &str, name: &str, src: &str, test: bool, exe: Option<&str>| {
            serde_json::json!({
                "reason": "compiler-artifact",
                "target": {"kind": [kind], "name": name, "src_path": src},
                "profile": {"test": test},
                "executable": exe,
            })
            .to_string()
        };
        let stdout = [
            r#"{"reason":"build-script-executed"}"#.to_string(),
            // A dependency, and this package's library: no program.
            artifact("lib", "dep", "/reg/dep/src/lib.rs", false, None),
            artifact("lib", "demo", "/p/src/lib.rs", false, None),
            artifact(
                "bin",
                "demo",
                "/p/src/main.rs",
                true,
                Some("/p/target/debug/deps/demo-1234"),
            ),
            artifact(
                "bin",
                "demo",
                "/p/src/main.rs",
                false,
                Some("/p/target/debug/demo"),
            ),
            // Another package's program of the same name.
            artifact(
                "bin",
                "demo",
                "/q/src/main.rs",
                false,
                Some("/q/target/debug/demo"),
            ),
        ]
        .join("\n");
        let target = |kind| CargoTarget {
            manifest: PathBuf::from("/p/Cargo.toml"),
            package: "demo".into(),
            kind,
            module: Vec::new(),
        };
        let bin = target(TargetKind::Bin("demo".into()));
        assert_eq!(
            built_executable(&stdout, &bin, false),
            Some(PathBuf::from("/p/target/debug/demo"))
        );
        assert_eq!(
            built_executable(&stdout, &bin, true),
            Some(PathBuf::from("/p/target/debug/deps/demo-1234"))
        );
        assert_eq!(
            built_executable(&stdout, &target(TargetKind::Lib), false),
            None
        );
    }

    /// A test is run by the name its binary lists: the one worked out, or
    /// the one test ending in its name.
    #[test]
    fn a_test_is_named_as_its_binary_lists_it() {
        let list = "parser::tests::adds: test\nparser::tests::subs: test\nother::adds: test\nbench: benchmark\n";
        assert_eq!(
            listed_test(list, "parser::tests::adds").as_deref(),
            Some("parser::tests::adds")
        );
        assert_eq!(
            listed_test(list, "tests::subs").as_deref(),
            Some("parser::tests::subs")
        );
        assert_eq!(listed_test(list, "adds"), None, "two end in it");
        assert_eq!(listed_test(list, "x::bench"), None, "not a test");
        // Another module's test of the same name is not the one meant.
        assert_eq!(listed_test(list, "cli::tests::adds"), None);
        assert_eq!(listed_test(list, "x::adds"), None);
        assert_eq!(
            test_args("a::b"),
            ["a::b", "--exact", "--include-ignored", "--nocapture"]
        );
    }

    /// A program Cargo built is told its package's folder, as `cargo run`
    /// and `cargo test` tell it.
    #[test]
    fn a_built_program_is_told_its_package_folder() {
        let mut launch = serde_json::json!({ "program": "/p/target/debug/app" });
        with_cargo_env(&mut launch, Path::new("/p"));
        assert_eq!(launch["env"], serde_json::json!(["CARGO_MANIFEST_DIR=/p"]));
        let mut launch = serde_json::json!({ "env": ["RUST_LOG=info"] });
        with_cargo_env(&mut launch, Path::new("/p"));
        assert_eq!(
            launch["env"],
            serde_json::json!(["RUST_LOG=info", "CARGO_MANIFEST_DIR=/p"])
        );
    }

    /// With Cargo on this machine: a crate's tests are built, the test
    /// binary Cargo reports is the one built, and it lists the test.
    #[test]
    fn a_crates_tests_are_built_and_listed_by_cargo() {
        let Some(cargo) = find_cargo() else {
            eprintln!("no cargo here: skipped");
            return;
        };
        let dir = TempDir::new("cargorun-build");
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        );
        let lib = write(
            root,
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() { assert_eq!(super::add(1, 2), 3); }\n}\n",
        );
        let target = target_of(&lib).unwrap();
        let mut lines = Vec::new();
        let exe = build(
            &cargo,
            &target,
            true,
            &Features::default(),
            |l| lines.push(l.to_string()),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|e| panic!("{e:?}: {lines:?}"));
        assert!(exe.is_file(), "{exe:?}");
        assert!(
            lines.iter().any(|l| l.contains("Compiling probe")),
            "{lines:?}"
        );
        let list = list_tests(&exe, root).unwrap();
        assert_eq!(
            listed_test(&list, &target.test_path(&["tests".into()], "adds")).as_deref(),
            Some("tests::adds")
        );
    }
}
