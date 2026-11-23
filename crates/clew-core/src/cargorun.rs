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

    /// Cargo's flags for building `target` with these features and the
    /// ones it requires (`required-features`): of the features named, its
    /// package's own — `pkg/feature` of its package, a bare one it declares.
    /// One chosen for another member of the workspace, passed to every
    /// member's build, failed them all ("the package does not contain this
    /// feature").
    fn args(&self, target: &CargoTarget) -> Vec<String> {
        let manifest = read_manifest(&target.manifest);
        let declared = manifest.as_ref().map(declared_features);
        let mut list: Vec<String> = Vec::new();
        for feature in &self.list {
            let own = match feature.split_once('/') {
                Some((package, name)) if package == target.package => Some(name.to_string()),
                Some(_) => None,
                None if declared
                    .as_ref()
                    .is_none_or(|d| d.contains(feature.as_str())) =>
                {
                    Some(feature.clone())
                }
                None => None,
            };
            if let Some(own) = own.filter(|f| !list.contains(f)) {
                list.push(own);
            }
        }
        if let Some(manifest) = &manifest {
            for feature in required_features(manifest, target) {
                if !list.contains(&feature) {
                    list.push(feature);
                }
            }
        }
        let mut out = Vec::new();
        if self.all {
            out.push("--all-features".into());
        } else if !list.is_empty() {
            out.push("--features".into());
            out.push(list.join(","));
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
        // Below a root the manifest names outside the usual folders —
        // `[lib] path = "lib.rs"` at the package's top — a module of that
        // target, from its root's folder: they were in no target, and had
        // no ▶.
        // The build script is no module of a target.
        _ if file
            == dir.join(
                value
                    .get("package")
                    .and_then(|p| p.get("build")?.as_str())
                    .unwrap_or("build.rs"),
            ) =>
        {
            None
        }
        _ => {
            let roots = declared("lib")
                .into_iter()
                .map(|(_, path)| (TargetKind::Lib, path))
                .chain(declared("bin").into_iter().map(|(name, path)| {
                    let name = name.unwrap_or_else(|| stem(&path));
                    (TargetKind::Bin(name), path)
                }));
            roots
                .filter_map(|(kind, root)| {
                    let below = parts_below(file, root.parent()?)?;
                    Some((kind, below))
                })
                .min_by_key(|(_, below)| below.len())
                .and_then(|(kind, below)| target(kind, module_of(&below)))
        }
    }
}

/// The program `cargo run` runs in a package: one of its binaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageProgram {
    /// Its `default-run`, or its only binary.
    One(CargoTarget),
    /// No binary: a library alone.
    None,
    /// Binaries, and no `default-run` to say which: their names.
    Several(Vec<String>),
}

/// The program `cargo run` runs in the package of `manifest`: its
/// `default-run`, or its only binary — `src/main.rs`, a `src/bin/` file
/// or folder, or a `[[bin]]` — as Cargo finds them.
pub fn package_program(manifest: &Path) -> PackageProgram {
    let Some(value) = read_manifest(manifest) else {
        return PackageProgram::None;
    };
    let Some(package) = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
    else {
        return PackageProgram::None;
    };
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let names = binaries(dir, &value, package);
    let default_run = value
        .get("package")
        .and_then(|p| p.get("default-run"))
        .and_then(|d| d.as_str());
    let chosen = match (default_run, names.as_slice()) {
        (Some(name), _) if names.iter().any(|n| n == name) => name.to_string(),
        (_, [one]) => one.clone(),
        (_, []) => return PackageProgram::None,
        _ => return PackageProgram::Several(names),
    };
    PackageProgram::One(CargoTarget {
        manifest: manifest.to_path_buf(),
        package: package.to_string(),
        kind: TargetKind::Bin(chosen),
        module: Vec::new(),
    })
}

/// The names of a package's binaries, as Cargo finds them: its `[[bin]]`s,
/// then — unless `autobins = false` — `src/main.rs` (named for the package)
/// and each `src/bin/x.rs` and `src/bin/x/main.rs` not already one of them.
fn binaries(dir: &Path, value: &toml::Value, package: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    for bin in value
        .get("bin")
        .and_then(|b| b.as_array())
        .into_iter()
        .flatten()
    {
        let Some(name) = bin.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        if let Some(path) = bin.get("path").and_then(|p| p.as_str()) {
            paths.push(dir.join(path));
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    let auto = value
        .get("package")
        .and_then(|p| p.get("autobins"))
        .and_then(|a| a.as_bool())
        != Some(false);
    if !auto {
        return names;
    }
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    let main = dir.join("src/main.rs");
    if main.is_file() {
        found.push((package.to_string(), main));
    }
    let mut in_bin: Vec<(String, PathBuf)> = std::fs::read_dir(dir.join("src/bin"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "rs") && path.is_file() {
                Some((path.file_stem()?.to_str()?.to_string(), path))
            } else if path.join("main.rs").is_file() {
                Some((
                    path.file_name()?.to_str()?.to_string(),
                    path.join("main.rs"),
                ))
            } else {
                None
            }
        })
        .collect();
    in_bin.sort();
    found.extend(in_bin);
    for (name, path) in found {
        if !names.contains(&name) && !paths.contains(&path) {
            names.push(name);
        }
    }
    names
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
/// package's. A `[[bin]]` with no `path` is at `src/main.rs` only when it is
/// named for the package: Cargo finds any other at `src/bin/<name>.rs`, and
/// taking that one for `src/main.rs` ran the wrong program.
fn default_bin(value: &toml::Value, package: &str) -> String {
    value
        .get("bin")
        .and_then(|b| b.as_array())
        .and_then(|bins| {
            bins.iter().find_map(|b| {
                let name = b.get("name")?.as_str()?;
                let path = b.get("path").and_then(|p| p.as_str());
                (path == Some("src/main.rs") || (path.is_none() && name == package))
                    .then(|| name.to_string())
            })
        })
        .unwrap_or_else(|| package.to_string())
}

/// A package's manifest, read (bounded) and parsed.
fn read_manifest(manifest: &Path) -> Option<toml::Value> {
    let text = crate::statefile::read_capped(manifest, MAX_READ_BYTES)?;
    toml::from_str::<toml::Value>(&text).ok()
}

/// The features a package declares: its `[features]`, and its optional
/// dependencies, each a feature of its name.
fn declared_features(manifest: &toml::Value) -> std::collections::HashSet<String> {
    let mut out: std::collections::HashSet<String> = manifest
        .get("features")
        .and_then(|f| f.as_table())
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();
    for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = manifest.get(table).and_then(|d| d.as_table()) {
            out.extend(
                deps.iter()
                    .filter(|(_, d)| d.get("optional").and_then(|o| o.as_bool()) == Some(true))
                    .map(|(name, _)| name.clone()),
            );
        }
    }
    out
}

/// The features `target` cannot be built without (`required-features` of
/// its `[[bin]]`, `[[example]]`, `[[test]]` or `[[bench]]`): Cargo skips
/// it, unbuilt, without them.
fn required_features(manifest: &toml::Value, target: &CargoTarget) -> Vec<String> {
    let (table, name) = match &target.kind {
        TargetKind::Bin(n) => ("bin", n),
        TargetKind::Example(n) => ("example", n),
        TargetKind::Test(n) => ("test", n),
        TargetKind::Bench(n) => ("bench", n),
        TargetKind::Lib => return Vec::new(),
    };
    manifest
        .get(table)
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
        .filter(|t| t.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))
        .filter_map(|t| t.get("required-features")?.as_array())
        .flatten()
        .filter_map(|f| f.as_str().map(str::to_string))
        .collect()
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
    args.extend(features.args(target));
    args
}

/// The executable Cargo reports, in its JSON messages `stdout`, having
/// built for `target`: the program, or (`test`) its test binary.
pub fn built_executable(stdout: &str, target: &CargoTarget, test: bool) -> Option<PathBuf> {
    artifact(stdout, target, test)
        .and_then(|message| message["executable"].as_str().map(PathBuf::from))
}

/// Cargo's `compiler-artifact` message, of its JSON messages `stdout`, for
/// the executable it built for `target`.
fn artifact(stdout: &str, target: &CargoTarget, test: bool) -> Option<serde_json::Value> {
    let package_dir = target.package_dir();
    stdout.lines().find_map(|line| {
        let message: serde_json::Value = serde_json::from_str(line).ok()?;
        if message["reason"] != "compiler-artifact" {
            return None;
        }
        message["executable"].as_str()?;
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
        matches.then_some(message)
    })
}

/// What Cargo tells a program it runs (`cargo run`, `cargo test`), as
/// variables to set on it, the later of two of a name the one set:
///
/// - `[env]` of the Cargo configuration found from the package's folder
///   ([`config_env`]);
/// - `CARGO`, `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`, and the
///   package's name and version (`CARGO_PKG_NAME`, `CARGO_PKG_VERSION`
///   and its parts) as Cargo built it, from its report `stdout`;
/// - where the libraries it loads are found ([`library_dirs`]): run without
///   them, a proc-macro crate's tests, or a program using a `dylib`, ended
///   before `main` without its toolchain's `libstd`.
pub fn program_env(
    cargo: &Path,
    target: &CargoTarget,
    executable: &Path,
    stdout: &str,
    test: bool,
) -> Vec<(String, String)> {
    let package_dir = target.package_dir();
    let cargo_home = cargo_home();
    let mut env = config_env(package_dir, cargo_home.as_deref(), &|name| {
        std::env::var_os(name).is_some()
    });
    let lossy = |p: &Path| p.to_string_lossy().into_owned();
    env.push(("CARGO".into(), lossy(cargo)));
    env.push(("CARGO_MANIFEST_DIR".into(), lossy(package_dir)));
    env.push(("CARGO_MANIFEST_PATH".into(), lossy(&target.manifest)));
    env.push(("CARGO_PKG_NAME".into(), target.package.clone()));
    let version = artifact(stdout, target, test)
        .and_then(|m| m["package_id"].as_str().and_then(package_version));
    if let Some(version) = version {
        env.extend(version_vars(&version));
    }
    let sysroot = sysroot_libdir(cargo, package_dir);
    let dirs = library_dirs(executable, stdout, sysroot.as_deref());
    let var = dylib_path_var();
    // The libraries' folders first, then where the program looked anyway:
    // on Windows the `PATH` its children get.
    let inherited = match var {
        "PATH" => Some(crate::linerpc::child_path()),
        _ => std::env::var_os(var),
    };
    let mut paths = dirs;
    let had = inherited.as_ref().is_some_and(|v| !v.is_empty());
    paths.extend(inherited.iter().flat_map(std::env::split_paths));
    // macOS's own fallback, which a set variable takes the place of.
    if var == "DYLD_FALLBACK_LIBRARY_PATH" && !had {
        paths.extend(crate::home_dir().map(|h| h.join("lib")));
        paths.extend([PathBuf::from("/usr/local/lib"), PathBuf::from("/usr/lib")]);
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        env.push((var.into(), joined.to_string_lossy().into_owned()));
    }
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(String, String)> = env
        .into_iter()
        .rev()
        .filter(|(k, _)| seen.insert(k.clone()))
        .collect();
    out.reverse();
    out
}

/// The variable the system finds a program's shared libraries by.
fn dylib_path_var() -> &'static str {
    if cfg!(windows) {
        "PATH"
    } else if cfg!(target_os = "macos") {
        "DYLD_FALLBACK_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

/// The version of a Cargo package id: `path+file:///p/app#0.1.0`,
/// `…#name@0.1.0`, or before Cargo 1.77 `app 0.1.0 (path+file:///p/app)`.
fn package_version(id: &str) -> Option<String> {
    let version = match id.rsplit_once('#') {
        Some((_, fragment)) => fragment.rsplit_once('@').map_or(fragment, |(_, v)| v),
        None => id.split_whitespace().nth(1)?,
    };
    version
        .starts_with(|c: char| c.is_ascii_digit())
        .then(|| version.to_string())
}

/// `CARGO_PKG_VERSION` and its parts, as Cargo sets them for `version`.
fn version_vars(version: &str) -> Vec<(String, String)> {
    let core = version.split('+').next().unwrap_or(version);
    let (numbers, pre) = core.split_once('-').unwrap_or((core, ""));
    let mut parts = numbers.split('.');
    let mut part = || parts.next().unwrap_or("").to_string();
    vec![
        ("CARGO_PKG_VERSION".into(), version.to_string()),
        ("CARGO_PKG_VERSION_MAJOR".into(), part()),
        ("CARGO_PKG_VERSION_MINOR".into(), part()),
        ("CARGO_PKG_VERSION_PATCH".into(), part()),
        ("CARGO_PKG_VERSION_PRE".into(), pre.to_string()),
    ]
}

/// Where a program Cargo built finds the shared libraries it loads, in the
/// order Cargo gives them: the folders build scripts link from that are in
/// the build's output (`cargo:rustc-link-search`, from the report
/// `stdout`), the dependencies' folder, the output folder, and the
/// toolchain's libraries (`sysroot`: its `libstd`, which a proc-macro
/// crate's tests load).
fn library_dirs(executable: &Path, stdout: &str, sysroot: Option<&Path>) -> Vec<PathBuf> {
    let Some(parent) = executable.parent() else {
        return Vec::new();
    };
    // `target/debug/deps/x-1a2b` (a test), `target/debug/examples/x`, or
    // `target/debug/x`.
    let output = match parent.file_name().and_then(|n| n.to_str()) {
        Some("deps" | "examples") => parent.parent().unwrap_or(parent),
        _ => parent,
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"] != "build-script-executed" {
            continue;
        }
        let paths = message["linked_paths"].as_array().into_iter().flatten();
        for path in paths.filter_map(|p| p.as_str()) {
            let path = match path.split_once('=') {
                Some(("native" | "crate" | "dependency" | "framework" | "all", p)) => p,
                _ => path,
            };
            let path = PathBuf::from(path);
            if path.starts_with(output) && !dirs.contains(&path) {
                dirs.push(path);
            }
        }
    }
    dirs.push(output.join("deps"));
    dirs.push(output.to_path_buf());
    dirs.extend(sysroot.map(Path::to_path_buf));
    dirs
}

/// The folder of the toolchain's libraries for the host (`rustc --print
/// target-libdir`), asked of the `rustc` beside `cargo` (rustup's, which
/// picks the toolchain of the package's folder as Cargo did), else the
/// first on the path.
fn sysroot_libdir(cargo: &Path, dir: &Path) -> Option<PathBuf> {
    let beside = cargo.with_file_name(format!("rustc{}", std::env::consts::EXE_SUFFIX));
    let rustc = if beside.is_file() {
        beside
    } else {
        crate::lsp::store::find_in_unresolved(&crate::linerpc::child_path(), "rustc")?
    };
    let out = crate::spawn::command(&rustc)
        .args(["--print", "target-libdir"])
        .current_dir(dir)
        .env("PATH", crate::linerpc::child_path())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let line = String::from_utf8(out.stdout).ok()?;
    let line = line.trim();
    (out.status.success() && !line.is_empty()).then(|| PathBuf::from(line))
}

/// The `[env]` of the Cargo configuration found from `dir` (each
/// `.cargo/config.toml`, or `.cargo/config`, in it and the folders above,
/// the nearest deciding), then of `cargo_home`'s: as Cargo sets it for a
/// program it runs. A variable already set (`is_set`) is kept unless the
/// entry says `force`; one marked `relative` is a path from the folder its
/// `.cargo` is in.
fn config_env(
    dir: &Path,
    cargo_home: Option<&Path>,
    is_set: &dyn Fn(&str) -> bool,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    // Farthest first, so a nearer file's entry is the one kept.
    for (config_dir, base) in config_dirs(dir, cargo_home).into_iter().rev() {
        let Some(value) = read_config(&config_dir) else {
            continue;
        };
        let Some(table) = value.get("env").and_then(|e| e.as_table()) else {
            continue;
        };
        for (name, entry) in table {
            let (value, force, relative) = match entry {
                toml::Value::String(v) => (v.clone(), false, false),
                toml::Value::Table(t) => {
                    let Some(v) = t.get("value").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let flag = |k: &str| t.get(k).and_then(|f| f.as_bool()) == Some(true);
                    (v.to_string(), flag("force"), flag("relative"))
                }
                _ => continue,
            };
            if is_set(name) && !force {
                continue;
            }
            let value = match relative {
                true => base.join(&value).to_string_lossy().into_owned(),
                false => value,
            };
            out.retain(|(k, _)| k != name);
            out.push((name.clone(), value));
        }
    }
    out
}

/// Cargo's home: `CARGO_HOME` (when absolute), else `~/.cargo`.
pub fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| crate::home_dir().map(|h| h.join(".cargo")))
}

/// The `.cargo` folders whose configuration applies in `dir`, the nearest
/// first — one in `dir` and in each folder above it, then `cargo_home` —
/// each with the folder it is in.
fn config_dirs(dir: &Path, cargo_home: Option<&Path>) -> Vec<(PathBuf, PathBuf)> {
    let mut dirs: Vec<(PathBuf, PathBuf)> = dir
        .ancestors()
        .map(|d| (d.join(".cargo"), d.to_path_buf()))
        .collect();
    if let Some(home) = cargo_home
        && !dirs.iter().any(|(c, _)| c == home)
    {
        dirs.push((
            home.to_path_buf(),
            home.parent().unwrap_or(home).to_path_buf(),
        ));
    }
    dirs
}

/// The configuration in the `.cargo` folder `config_dir`: its `config`,
/// which Cargo reads where both are, else its `config.toml`.
fn read_config(config_dir: &Path) -> Option<toml::Value> {
    let text = crate::statefile::read_capped(&config_dir.join("config"), MAX_READ_BYTES).or_else(
        || crate::statefile::read_capped(&config_dir.join("config.toml"), MAX_READ_BYTES),
    )?;
    toml::from_str(&text).ok()
}

/// The target `cargo build` builds for in `dir` when none is named: the
/// `build.target` of the nearest Cargo configuration that sets one, as
/// rust-analyzer reads it. `None` for none, or for several
/// (`target = ["a", "b"]`), which no one reading is.
pub fn config_build_target(dir: &Path, cargo_home: Option<&Path>) -> Option<String> {
    // The nearest that sets one decides, whatever it sets.
    let target = config_dirs(dir, cargo_home)
        .into_iter()
        .find_map(|(config_dir, _)| {
            read_config(&config_dir)?
                .get("build")?
                .get("target")
                .cloned()
        })?;
    match target {
        toml::Value::String(t) => Some(t),
        toml::Value::Array(list) if list.len() == 1 => list[0].as_str().map(str::to_string),
        _ => None,
    }
}

/// Cargo's arguments for the doctests of `target`'s package —
/// `cargo test --doc`, with the features its build has — then `harness`
/// for rustdoc's test runner. A package's doctests are its library's.
pub fn doctest_args(target: &CargoTarget, features: &Features, harness: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "test".into(),
        "--doc".into(),
        "--manifest-path".into(),
        target.manifest.to_string_lossy().into_owned(),
        "--color".into(),
        "never".into(),
    ];
    args.extend(features.args(target));
    args.push("--".into());
    args.extend(harness.iter().map(|a| a.to_string()));
    args
}

/// Of rustdoc's list of a package's doctests (`--list --format terse`:
/// `src/lib.rs - add (line 22): test`), the one in the file `rel` (from its
/// package's folder) whose example opens on 1-based `line`.
pub fn listed_doctest(list: &str, rel: &str, line: usize) -> Option<String> {
    let rel = rel.replace('\\', "/");
    let head = format!("{rel} - ");
    let tail = format!(" (line {line})");
    list.lines()
        .filter_map(|l| l.strip_suffix(": test"))
        .find(|name| {
            let name = name.replace('\\', "/");
            name.starts_with(&head) && name.ends_with(&tail)
        })
        .map(str::to_string)
}

/// What rustdoc's test runner is given to run the one doctest `name` of
/// the doctests `list`ed: rustdoc splits what it is given at spaces, and a
/// doctest's name has them, so no filter can be the name, nor `--exact`
/// meet it. A word of it that is no space's: the item's path (`Thing::new`)
/// or its line's (`22)`); then, for each other doctest that word reaches
/// too (an item's second example), `--skip` with a word of that one's that
/// is not in this one's.
pub fn doctest_filter(list: &str, name: &str) -> Vec<String> {
    let names: Vec<&str> = list
        .lines()
        .filter_map(|l| l.strip_suffix(": test"))
        .collect();
    let path = name
        .split_once(" - ")
        .and_then(|(_, rest)| rest.rsplit_once(" (line "))
        .map(|(path, _)| path)
        .filter(|p| !p.is_empty() && !p.contains(char::is_whitespace));
    let line = name.rsplit(' ').next().filter(|w| w.ends_with(')'));
    let Some(filter) = path.or(line) else {
        return vec![name.to_string()];
    };
    let mut args = vec![filter.to_string()];
    for other in names.iter().filter(|o| **o != name && o.contains(filter)) {
        // Its own line's word first: `30)` in `… add (line 30)`.
        let skip = other.split_whitespace().rev().find(|w| !name.contains(w));
        if let Some(word) = skip {
            args.extend(["--skip".to_string(), word.to_string()]);
        }
    }
    args
}

/// How a run of one doctest ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoctestEnd {
    /// Cargo ran to this end: the doctest's run, or a build that failed
    /// before it.
    Ran(crate::spawn::RunEnd),
    /// The package's doctests were listed, and there is none at the line:
    /// the example is in a binary's file, or the file is no module of the
    /// library.
    NotListed,
}

/// Run the one doctest of the file `file` whose example opens on 1-based
/// `line`, in the package of `target`, as `cargo test --doc` runs it: its
/// doctests listed (the library built), the one there found by its name,
/// then run alone. Each line Cargo and the test write is handed to `said`
/// with whether it went to stderr. Blocking; stopped once `cancel` is set.
pub fn run_doctest(
    cargo: &Path,
    target: &CargoTarget,
    features: &Features,
    file: &Path,
    line: usize,
    mut said: impl FnMut(&str, bool),
    cancel: &AtomicBool,
) -> Result<DoctestEnd, String> {
    use crate::spawn::RunEnd;
    let rel = file
        .strip_prefix(target.package_dir())
        .map_err(|_| "the file is not in its package's folder".to_string())?
        .to_string_lossy()
        .into_owned();
    let command = |harness: &[&str]| {
        let mut command = crate::spawn::command(cargo);
        command
            .args(doctest_args(target, features, harness))
            .current_dir(target.package_dir())
            .env("PATH", crate::linerpc::child_path());
        command
    };
    let mut list = String::new();
    let listed = crate::spawn::run_streaming(
        command(&["--list", "--format", "terse"]),
        |l, stderr| match stderr {
            true => said(l, true),
            false => {
                list.push_str(l);
                list.push('\n');
            }
        },
        cancel,
    )?;
    if listed != RunEnd::Exited(0) {
        return Ok(DoctestEnd::Ran(listed));
    }
    let Some(name) = listed_doctest(&list, &rel, line) else {
        return Ok(DoctestEnd::NotListed);
    };
    let filter = doctest_filter(&list, &name);
    let filter: Vec<&str> = filter.iter().map(String::as_str).collect();
    let ran = crate::spawn::run_streaming(command(&filter), said, cancel)?;
    Ok(DoctestEnd::Ran(ran))
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

/// Of a test binary's `--list --format terse` lines, the tests of a group,
/// in the order listed: those named one of `under` or below it — a module
/// (`parser::tests`), or a test at its file's root. Every test where
/// `under` is empty.
pub fn listed_under(list: &str, under: &[String]) -> Vec<String> {
    list.lines()
        .filter_map(|l| l.strip_suffix(": test"))
        .filter(|test| {
            under.is_empty()
                || under.iter().any(|u| {
                    test.strip_prefix(u.as_str())
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
                })
        })
        .map(str::to_string)
        .collect()
}

/// The arguments a test binary runs the group `under` with, `selected` the
/// tests of its `list` in it ([`listed_under`]): none, for every test; the
/// one filter its module makes (`parser::tests::`) where that picks those
/// tests and no other; else each by its exact name — the filter is matched
/// anywhere in a name, and `a::tests::` also ran `b::a::tests::x`. Its
/// ignored tests stay ignored, as `cargo test` leaves them.
pub fn group_args(list: &str, under: &[String], selected: &[String]) -> Vec<String> {
    if under.is_empty() {
        return Vec::new();
    }
    if let [module] = under {
        let filter = format!("{module}::");
        let picked = list
            .lines()
            .filter_map(|l| l.strip_suffix(": test"))
            .filter(|t| t.contains(&filter))
            .count();
        if picked == selected.len() && !selected.iter().any(|t| t == module) {
            return vec![filter];
        }
    }
    let mut args = selected.to_vec();
    args.push("--exact".into());
    args
}

/// The toolchain the project at `dir` pins for rustup — the channel of a
/// `rust-toolchain` or `rust-toolchain.toml` in it or a folder above, as
/// rustup reads them (the first where both are) — by its name only: a
/// toolchain named by `path` is the repository's own program, never run for
/// it, and so is no channel here.
pub fn pinned_toolchain(dir: &Path) -> Option<String> {
    for d in dir.ancestors() {
        for name in ["rust-toolchain", "rust-toolchain.toml"] {
            let Some(text) = crate::statefile::read_capped(&d.join(name), 64 * 1024) else {
                continue;
            };
            let channel = match toml::from_str::<toml::Value>(&text) {
                Ok(v) => v
                    .get("toolchain")
                    .and_then(|t| t.get("channel"))
                    .and_then(|c| c.as_str())
                    .map(str::to_string),
                // The old form: the channel alone.
                Err(_) => text
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .map(str::to_string),
            };
            // The file found decides, a channel named well or not.
            return channel.filter(|c| {
                !c.is_empty()
                    && c.len() <= 64
                    && c.chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
            });
        }
    }
    None
}

/// Whether the toolchain of the project at `dir` has Rust's own sources
/// (`rust-src`), which rust-analyzer reads the standard library from —
/// without them `Vec`, `Option` and the rest are names it cannot follow.
/// `Some(Err(toolchain))` when they are missing and rustup can add them (to
/// `toolchain`, the project's pinned one, or its default); `Some(Ok(()))`
/// when they are there; `None` with no rustup toolchain to ask. Never
/// installs a toolchain to answer. Blocking.
pub fn rust_src(dir: &Path) -> Option<Result<(), Option<String>>> {
    let path = crate::linerpc::child_path();
    crate::lsp::store::find_in_unresolved(&path, "rustup")?;
    let rustc = crate::lsp::store::find_in_unresolved(&path, "rustc")?;
    let toolchain = pinned_toolchain(dir);
    let mut command = crate::spawn::command(&rustc);
    command
        .args(["--print", "sysroot"])
        .current_dir(crate::linerpc::neutral_dir())
        .env("PATH", &path)
        .env("RUSTUP_AUTO_INSTALL", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(t) = &toolchain {
        command.env("RUSTUP_TOOLCHAIN", t);
    }
    let out = command.output().ok().filter(|o| o.status.success())?;
    let sysroot = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    Some(
        match sysroot.join("lib/rustlib/src/rust/library").is_dir() {
            true => Ok(()),
            false => Err(toolchain),
        },
    )
}

/// Add Rust's own sources to `toolchain` (its default where `None`):
/// `rustup component add rust-src`. What rustup said why, where it could
/// not. Blocking: it downloads them.
pub fn add_rust_src(toolchain: Option<&str>) -> Result<(), String> {
    let path = crate::linerpc::child_path();
    let rustup = crate::lsp::store::find_in_unresolved(&path, "rustup")
        .ok_or_else(|| "rustup was not found".to_string())?;
    let mut command = crate::spawn::command(&rustup);
    command
        .args(["component", "add", "rust-src"])
        .current_dir(crate::linerpc::neutral_dir())
        .env("PATH", &path)
        .stdin(Stdio::null());
    if let Some(t) = toolchain {
        command.args(["--toolchain", t]);
    }
    let out = command.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&out.stderr);
    let why = said
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("rustup failed")
        .to_string();
    Err(why)
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

/// A program Cargo built, to run as Cargo runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    pub executable: PathBuf,
    /// The variables Cargo sets on a program it runs ([`program_env`]).
    pub env: Vec<(String, String)>,
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
) -> Result<Built, BuildError> {
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
    let executable = built_executable(&out, target, test).ok_or(BuildError::NoProgram)?;
    let env = program_env(cargo, target, &executable, &out, test);
    Ok(Built { executable, env })
}

/// The tests a test binary has, as `--list --format terse` lists them;
/// run in `cwd` with the variables `env` ([`Built::env`]).
pub fn list_tests(executable: &Path, cwd: &Path, env: &[(String, String)]) -> Option<String> {
    let mut child = crate::spawn::command(executable)
        .args(["--list", "--format", "terse"])
        .envs(env.iter().map(|(k, v)| (k, v)))
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

/// A launch body (lldb-dap's) with what Cargo tells a program it runs
/// ([`Built::env`]) — its package's folder, `CARGO_MANIFEST_DIR`, where
/// snapshot tests and fixtures are found by; where its libraries are —
/// added to the variables the body sets: a list of `VAR=VALUE`, as
/// lldb-dap takes them, or a table of them. One the body already sets
/// keeps its value.
pub fn with_cargo_env(launch: &mut serde_json::Value, env: &[(String, String)]) {
    let Some(body) = launch.as_object_mut() else {
        return;
    };
    match body
        .entry("env")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
    {
        serde_json::Value::Array(list) => {
            let named = |entry: &serde_json::Value, name: &str| {
                entry
                    .as_str()
                    .and_then(|e| e.split_once('='))
                    .is_some_and(|(k, _)| k == name)
            };
            for (name, value) in env {
                if !list.iter().any(|e| named(e, name)) {
                    list.push(serde_json::Value::String(format!("{name}={value}")));
                }
            }
        }
        serde_json::Value::Object(table) => {
            for (name, value) in env {
                table
                    .entry(name.clone())
                    .or_insert_with(|| serde_json::Value::String(value.clone()));
            }
        }
        _ => {}
    }
}

/// The arguments a test binary, its tests listed as `list`, runs the test
/// the reader meant (`wanted`, as [`listed_test`] takes it) with: the one
/// test ([`test_args`]); else, a test that is many (`#[rstest]` cases,
/// `#[test_case]`s, listed as `adds::case_1`, below its name), all of
/// them, one at a time, their output as it comes. `None` when the build
/// has no such test.
pub fn one_test_args(list: &str, wanted: &str) -> Option<Vec<String>> {
    if let Some(found) = listed_test(list, wanted) {
        return Some(test_args(&found));
    }
    let under = [wanted.to_string()];
    let cases = listed_under(list, &under);
    if cases.is_empty() {
        return None;
    }
    let mut args = group_args(list, &under, &cases);
    args.extend([
        "--include-ignored".into(),
        "--nocapture".into(),
        "--test-threads=1".into(),
    ]);
    Some(args)
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

    /// A group's tests are the ones under its module, or every test of
    /// the target; run by the module's filter where it picks them alone,
    /// else each by its exact name, ignored ones left ignored.
    #[test]
    fn a_groups_tests_run_by_their_module_or_by_name() {
        let list = "a::tests::one: test\na::tests::two: test\nb::a::tests::x: test\n\
                    tests::root: test\nlone: test\nbench_it: bench\n";
        let under = |u: &[&str]| u.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            listed_under(list, &under(&["a::tests"])),
            ["a::tests::one", "a::tests::two"]
        );
        assert_eq!(listed_under(list, &[]).len(), 5, "every test, no bench");
        assert_eq!(
            listed_under(list, &under(&["tests", "lone"])),
            ["tests::root", "lone"]
        );
        assert!(listed_under(list, &under(&["a::test"])).is_empty());

        // `b::tests::` is found in no other name: the filter alone.
        let b = under(&["b::a::tests"]);
        assert_eq!(
            group_args(list, &b, &listed_under(list, &b)),
            ["b::a::tests::"]
        );
        // `a::tests::` is found in `b::a::tests::x` too: by name.
        let a = under(&["a::tests"]);
        assert_eq!(
            group_args(list, &a, &listed_under(list, &a)),
            ["a::tests::one", "a::tests::two", "--exact"]
        );
        let root = under(&["tests", "lone"]);
        assert_eq!(
            group_args(list, &root, &listed_under(list, &root)),
            ["tests::root", "lone", "--exact"]
        );
        assert!(group_args(list, &[], &listed_under(list, &[])).is_empty());
    }

    fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A file is placed in the target Cargo builds it in, by the manifest's
    /// paths and then Cargo's own layout, with its module there.
    /// A library rooted outside `src/` (`[lib] path = "lib.rs"`): its
    /// modules are its, from its root's folder; the build script is none.
    #[test]
    fn a_library_rooted_at_the_top_has_its_modules() {
        let dir = TempDir::new("cargorun-top-lib");
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"top\"\n\n[lib]\npath = \"lib.rs\"\n",
        );
        for rel in [
            "lib.rs",
            "util.rs",
            "net/mod.rs",
            "net/tcp.rs",
            "build.rs",
            "tests/it.rs",
        ] {
            write(root, rel, "");
        }
        let at = |rel: &str| target_of(&root.join(rel)).map(|t| (t.kind, t.module.join("::")));
        assert_eq!(at("lib.rs"), Some((TargetKind::Lib, String::new())));
        assert_eq!(at("util.rs"), Some((TargetKind::Lib, "util".into())));
        assert_eq!(at("net/mod.rs"), Some((TargetKind::Lib, "net".into())));
        assert_eq!(at("net/tcp.rs"), Some((TargetKind::Lib, "net::tcp".into())));
        assert_eq!(at("build.rs"), None);
        assert_eq!(
            at("tests/it.rs"),
            Some((TargetKind::Test("it".into()), String::new()))
        );
    }

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
                "fast,serde",
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
            .args(&target),
            ["--all-features"]
        );
    }

    /// In a workspace, a member is built with its own features only — one
    /// chosen for another member (`core/fast`), or a bare one it does not
    /// declare, failed its build — and with the features its target
    /// requires; a `[[bin]]` with no path is `src/main.rs` only when it is
    /// named for the package.
    #[test]
    fn a_member_is_built_with_its_own_and_its_required_features() {
        let dir = TempDir::new("cargorun-member-features");
        let root = dir.path();
        write(
            root,
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
             [features]\nextra = []\n\n\
             [dependencies]\nlog = { version = \"0.4\", optional = true }\n\n\
             [[bin]]\nname = \"alpha\"\nrequired-features = [\"extra\"]\n\n\
             [[test]]\nname = \"gated\"\nrequired-features = [\"extra\"]\n",
        );
        write(root, "app/src/main.rs", "fn main() {}\n");
        write(root, "app/src/bin/alpha.rs", "fn main() {}\n");
        write(root, "app/tests/gated.rs", "");
        let features = Features {
            all: false,
            list: vec![
                "core/fast".into(),
                "app/extra".into(),
                "fast".into(),
                "log".into(),
            ],
            no_default: false,
        };
        let main = target_of(&root.join("app/src/main.rs")).unwrap();
        assert_eq!(
            main.kind,
            TargetKind::Bin("app".into()),
            "main.rs is app, not alpha"
        );
        assert_eq!(features.args(&main), ["--features", "extra,log"]);
        let gated = target_of(&root.join("app/tests/gated.rs")).unwrap();
        assert_eq!(
            Features::default().args(&gated),
            ["--features", "extra"],
            "what the test requires"
        );
    }

    /// The program `cargo run` runs in a package: its only binary, wherever
    /// Cargo finds it, or its `default-run`; none in a library; for several
    /// with no `default-run`, their names.
    #[test]
    fn a_packages_program_is_the_one_cargo_run_runs() {
        let dir = TempDir::new("cargorun-package-program");
        let root = dir.path();
        let only = write(root, "only/Cargo.toml", "[package]\nname = \"only\"\n");
        write(root, "only/src/lib.rs", "");
        write(root, "only/src/bin/tool.rs", "fn main() {}\n");
        let kind = |p: PackageProgram| match p {
            PackageProgram::One(t) => Ok(t.kind),
            other => Err(other),
        };
        assert_eq!(
            kind(package_program(&only)),
            Ok(TargetKind::Bin("tool".into()))
        );
        let lib = write(root, "lib/Cargo.toml", "[package]\nname = \"lib\"\n");
        write(root, "lib/src/lib.rs", "");
        assert_eq!(kind(package_program(&lib)), Err(PackageProgram::None));
        let many = write(
            root,
            "many/Cargo.toml",
            "[package]\nname = \"many\"\n\n[[bin]]\nname = \"server\"\npath = \"src/server.rs\"\n",
        );
        write(root, "many/src/server.rs", "fn main() {}\n");
        write(root, "many/src/main.rs", "fn main() {}\n");
        write(root, "many/src/bin/cli/main.rs", "fn main() {}\n");
        assert_eq!(
            kind(package_program(&many)),
            Err(PackageProgram::Several(vec![
                "server".into(),
                "many".into(),
                "cli".into()
            ]))
        );
        let chosen = write(
            root,
            "chosen/Cargo.toml",
            "[package]\nname = \"chosen\"\ndefault-run = \"cli\"\n",
        );
        write(root, "chosen/src/main.rs", "fn main() {}\n");
        write(root, "chosen/src/bin/cli.rs", "fn main() {}\n");
        assert_eq!(
            kind(package_program(&chosen)),
            Ok(TargetKind::Bin("cli".into()))
        );
        let off = write(
            root,
            "off/Cargo.toml",
            "[package]\nname = \"off\"\nautobins = false\n",
        );
        write(root, "off/src/main.rs", "fn main() {}\n");
        assert_eq!(kind(package_program(&off)), Err(PackageProgram::None));
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

    /// A program Cargo built is told what Cargo tells it — its package's
    /// folder and the rest — in the form the launch body has; a variable
    /// the body sets keeps its value.
    #[test]
    fn a_built_program_is_told_what_cargo_tells_it() {
        let env = vec![
            ("CARGO_MANIFEST_DIR".to_string(), "/p".to_string()),
            ("RUST_LOG".to_string(), "warn".to_string()),
        ];
        let mut launch = serde_json::json!({ "program": "/p/target/debug/app" });
        with_cargo_env(&mut launch, &env);
        assert_eq!(
            launch["env"],
            serde_json::json!(["CARGO_MANIFEST_DIR=/p", "RUST_LOG=warn"])
        );
        let mut launch = serde_json::json!({ "env": ["RUST_LOG=info"] });
        with_cargo_env(&mut launch, &env);
        assert_eq!(
            launch["env"],
            serde_json::json!(["RUST_LOG=info", "CARGO_MANIFEST_DIR=/p"])
        );
        let mut launch = serde_json::json!({ "env": { "RUST_LOG": "info" } });
        with_cargo_env(&mut launch, &env);
        assert_eq!(
            launch["env"],
            serde_json::json!({ "RUST_LOG": "info", "CARGO_MANIFEST_DIR": "/p" })
        );
    }

    /// The package's version as Cargo built it, from either form of package
    /// id, and its parts as Cargo names them.
    #[test]
    fn a_program_is_told_its_packages_version() {
        assert_eq!(
            package_version("path+file:///p/app#0.1.0").as_deref(),
            Some("0.1.0")
        );
        assert_eq!(
            package_version("path+file:///p/crates/x#app@1.2.3-rc.1").as_deref(),
            Some("1.2.3-rc.1")
        );
        assert_eq!(
            package_version("app 0.4.0 (path+file:///p/app)").as_deref(),
            Some("0.4.0")
        );
        assert_eq!(package_version("nonsense"), None);
        let vars = version_vars("1.2.3-rc.1+build.5");
        let get = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("CARGO_PKG_VERSION"), Some("1.2.3-rc.1+build.5"));
        assert_eq!(get("CARGO_PKG_VERSION_MAJOR"), Some("1"));
        assert_eq!(get("CARGO_PKG_VERSION_MINOR"), Some("2"));
        assert_eq!(get("CARGO_PKG_VERSION_PATCH"), Some("3"));
        assert_eq!(get("CARGO_PKG_VERSION_PRE"), Some("rc.1"));
    }

    /// A program finds its libraries where `cargo run` and `cargo test`
    /// have it look: the build scripts' folders in the build's output (not
    /// the system's), the dependencies, the output, the toolchain's.
    #[test]
    fn a_program_finds_its_libraries_where_cargo_has_it_look() {
        let report = [
            r#"{"reason":"build-script-executed","linked_paths":["native=/p/target/debug/build/z-1/out/lib","/usr/lib/x86_64-linux-gnu","/p/target/debug/build/y-2/out"]}"#,
            r#"{"reason":"compiler-artifact","executable":null}"#,
            "not json",
        ]
        .join("\n");
        let sysroot = Path::new("/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib");
        let expected = [
            "/p/target/debug/build/z-1/out/lib",
            "/p/target/debug/build/y-2/out",
            "/p/target/debug/deps",
            "/p/target/debug",
            "/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib",
        ]
        .map(PathBuf::from);
        let test = Path::new("/p/target/debug/deps/app-1a2b");
        assert_eq!(library_dirs(test, &report, Some(sysroot)), expected);
        let program = Path::new("/p/target/debug/app");
        assert_eq!(library_dirs(program, &report, Some(sysroot)), expected);
        let example = Path::new("/p/target/debug/examples/demo");
        assert_eq!(library_dirs(example, &report, Some(sysroot)), expected);
    }

    /// `[env]` of the Cargo configuration, as Cargo sets it for a program it
    /// runs: the nearest file's entry, Cargo's home's last; a variable
    /// already set kept unless forced; a relative one from its folder.
    /// A pinned channel is read from either file, the nearest first; a
    /// toolchain named by a path, or by nothing a channel is, is none.
    #[test]
    fn the_projects_pinned_toolchain_is_its_channel() {
        let dir = TempDir::new("cargorun-toolchain");
        let root = dir.path();
        write(
            root,
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.80.0\"\ncomponents = [\"rust-src\"]\n",
        );
        write(root, "old/rust-toolchain", "nightly-2024-05-01\n");
        write(
            root,
            "pathed/rust-toolchain.toml",
            "[toolchain]\npath = \"/tmp/evil\"\n",
        );
        write(
            root,
            "odd/rust-toolchain.toml",
            "[toolchain]\nchannel = \"../x y\"\n",
        );
        std::fs::create_dir_all(root.join("deep/er")).unwrap();
        assert_eq!(pinned_toolchain(root).as_deref(), Some("1.80.0"));
        assert_eq!(
            pinned_toolchain(&root.join("deep/er")).as_deref(),
            Some("1.80.0")
        );
        assert_eq!(
            pinned_toolchain(&root.join("old")).as_deref(),
            Some("nightly-2024-05-01")
        );
        assert_eq!(pinned_toolchain(&root.join("pathed")), None);
        assert_eq!(pinned_toolchain(&root.join("odd")), None);
    }

    #[test]
    fn the_build_target_is_the_nearest_configurations() {
        let dir = TempDir::new("cargorun-build-target");
        let root = dir.path();
        write(
            root,
            ".cargo/config.toml",
            "[build]\ntarget = \"wasm32-unknown-unknown\"\n",
        );
        write(root, "a/.cargo/config.toml", "[build]\njobs = 2\n");
        write(
            root,
            "b/.cargo/config.toml",
            "[build]\ntarget = [\"x86_64-pc-windows-msvc\"]\n",
        );
        write(
            root,
            "c/.cargo/config.toml",
            "[build]\ntarget = [\"a\", \"b\"]\n",
        );
        let target = |d: &str| config_build_target(&root.join(d), None);
        assert_eq!(target("a").as_deref(), Some("wasm32-unknown-unknown"));
        assert_eq!(target("b").as_deref(), Some("x86_64-pc-windows-msvc"));
        assert_eq!(target("c"), None, "several: none is the one");
    }

    #[test]
    fn a_program_gets_the_env_of_cargos_configuration() {
        let dir = TempDir::new("cargorun-config-env");
        let root = dir.path();
        write(
            root,
            ".cargo/config.toml",
            "[env]\nSHARED = \"outer\"\nFIXTURES = { value = \"fixtures\", relative = true }\n\
             SET = \"config\"\nFORCED = { value = \"config\", force = true }\n",
        );
        write(
            root,
            "app/.cargo/config.toml",
            "[env]\nSHARED = \"inner\"\n",
        );
        // Where both are, Cargo reads `config`.
        write(root, "app/crate/.cargo/config", "[env]\nOLD = \"old\"\n");
        write(
            root,
            "app/crate/.cargo/config.toml",
            "[env]\nNEW = \"unread\"\n",
        );
        write(
            root,
            "home/config.toml",
            "[env]\nHOME_ONLY = \"home\"\nSHARED = \"home\"\n",
        );
        let is_set = |name: &str| matches!(name, "SET" | "FORCED");
        let env = config_env(&root.join("app/crate"), Some(&root.join("home")), &is_set);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("SHARED").as_deref(), Some("inner"), "{env:?}");
        assert_eq!(get("OLD").as_deref(), Some("old"));
        assert_eq!(get("NEW"), None);
        assert_eq!(get("HOME_ONLY").as_deref(), Some("home"));
        assert_eq!(get("SET"), None, "already set");
        assert_eq!(get("FORCED").as_deref(), Some("config"));
        assert_eq!(
            get("FIXTURES").map(PathBuf::from),
            Some(root.join("fixtures"))
        );
    }

    /// A test that is many — `#[rstest]` cases, `#[test_case]`s — runs as
    /// all of them, one at a time; one test by its exact name.
    #[test]
    fn a_test_with_cases_runs_them_all() {
        let list = "tests::adds::case_1: test\ntests::adds::case_2: test\ntests::adds_more: test\ntests::subs: test\n";
        assert_eq!(
            one_test_args(list, "tests::subs").unwrap(),
            test_args("tests::subs")
        );
        assert_eq!(
            one_test_args(list, "tests::adds").unwrap(),
            [
                "tests::adds::",
                "--include-ignored",
                "--nocapture",
                "--test-threads=1"
            ]
        );
        assert_eq!(one_test_args(list, "tests::muls"), None);
    }

    /// A doctest is found in rustdoc's list by its file and the line its
    /// example opens on — a module's own too — and by no other's.
    #[test]
    fn a_doctest_is_found_by_its_file_and_line() {
        let list = "src/lib.rs - (line 3): test\nsrc/lib.rs - Thing::new (line 16): test\n\
                    src/a/b.rs - a::b::G::h (line 4): test\nsrc/lib.rs - add (line 160): test\n";
        assert_eq!(
            listed_doctest(list, "src/lib.rs", 3).as_deref(),
            Some("src/lib.rs - (line 3)")
        );
        assert_eq!(
            listed_doctest(list, "src/lib.rs", 16).as_deref(),
            Some("src/lib.rs - Thing::new (line 16)")
        );
        assert_eq!(
            listed_doctest(list, "src/a/b.rs", 4).as_deref(),
            Some("src/a/b.rs - a::b::G::h (line 4)")
        );
        assert_eq!(
            listed_doctest(list, "src/lib.rs", 60),
            None,
            "not line 160's"
        );
        // Run by a word of its name, skipping the others it reaches.
        let two = "src/lib.rs - add (line 2): test\nsrc/lib.rs - add (line 9): test\n\
                   src/lib.rs - add_all (line 20): test\nsrc/lib.rs - (line 1): test\n";
        assert_eq!(
            doctest_filter(two, "src/lib.rs - add (line 2)"),
            ["add", "--skip", "9)", "--skip", "20)"]
        );
        assert_eq!(doctest_filter(two, "src/lib.rs - (line 1)"), ["1)"]);
        assert_eq!(listed_doctest(list, "src/a.rs", 4), None);
        assert_eq!(
            listed_doctest(
                "src\\a\\b.rs - a::b::G::h (line 4): test\n",
                "src/a/b.rs",
                4
            )
            .as_deref(),
            Some("src\\a\\b.rs - a::b::G::h (line 4)")
        );
    }

    /// With Cargo on this machine: one doctest runs alone, found by where
    /// its example is; a line with none is said to have none.
    #[test]
    fn one_doctest_runs_alone() {
        let Some(cargo) = find_cargo() else {
            eprintln!("no cargo here: skipped");
            return;
        };
        let dir = TempDir::new("cargorun-doctest");
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"dt\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        );
        let lib = write(
            root,
            "src/lib.rs",
            "/// Adds.\n/// ```\n/// assert_eq!(dt::add(1, 2), 3);\n/// ```\npub fn add(a: i32, b: i32) -> i32 { a + b }\n\
             /// Fails.\n/// ```\n/// assert_eq!(1, 2);\n/// ```\n/// ```\n/// assert!(true);\n/// ```\npub fn bad() {}\n",
        );
        let target = target_of(&lib).expect("a target");
        let run = |line: usize| {
            let mut out = String::new();
            let end = run_doctest(
                &cargo,
                &target,
                &Features::default(),
                &lib,
                line,
                |l, _| {
                    out.push_str(l);
                    out.push('\n');
                },
                &AtomicBool::new(false),
            );
            (end, out)
        };
        let (end, out) = run(2);
        assert_eq!(
            end,
            Ok(DoctestEnd::Ran(crate::spawn::RunEnd::Exited(0))),
            "{out}"
        );
        assert!(
            out.contains("1 passed") && out.contains("add (line 2)"),
            "{out}"
        );
        let (end, out) = run(7);
        assert_eq!(
            end,
            Ok(DoctestEnd::Ran(crate::spawn::RunEnd::Exited(101))),
            "{out}"
        );
        assert!(
            out.contains("bad (line 7)") && !out.contains("add (line 2)"),
            "{out}"
        );
        // The same item's other example runs alone.
        let (end, out) = run(10);
        assert_eq!(
            end,
            Ok(DoctestEnd::Ran(crate::spawn::RunEnd::Exited(0))),
            "{out}"
        );
        assert!(
            out.contains("1 passed") && !out.contains("bad (line 7)"),
            "{out}"
        );
        assert_eq!(run(5).0, Ok(DoctestEnd::NotListed));
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
        // A proc-macro crate: its test binary loads the toolchain's `libstd`.
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.3.1\"\nedition = \"2021\"\n\n\
             [lib]\nproc-macro = true\n\n[workspace]\n",
        );
        let lib = write(
            root,
            "src/lib.rs",
            "use proc_macro::TokenStream;\n\n#[proc_macro]\npub fn same(t: TokenStream) -> TokenStream { t }\n\n\
             #[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() { assert_eq!(1 + 2, 3); }\n}\n",
        );
        let target = target_of(&lib).unwrap();
        let mut lines = Vec::new();
        let built = build(
            &cargo,
            &target,
            true,
            &Features::default(),
            |l| lines.push(l.to_string()),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|e| panic!("{e:?}: {lines:?}"));
        let exe = &built.executable;
        assert!(exe.is_file(), "{exe:?}");
        assert!(
            lines.iter().any(|l| l.contains("Compiling probe")),
            "{lines:?}"
        );
        let get = |k: &str| {
            built
                .env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("CARGO_PKG_VERSION"), Some("0.3.1"), "{:?}", built.env);
        assert_eq!(get("CARGO_PKG_NAME"), Some("probe"));
        let list = list_tests(exe, root, &built.env).unwrap();
        assert_eq!(
            listed_test(&list, &target.test_path(&["tests".into()], "adds")).as_deref(),
            Some("tests::adds")
        );
        // Without the folders Cargo has it look in, it cannot start (here,
        // where `cargo test` did not give this process them already).
        let dirs = library_dirs(exe, "", sysroot_libdir(&cargo, root).as_deref());
        let lists = |path: Option<std::ffi::OsString>| {
            let mut command = crate::spawn::command(exe);
            command
                .args(["--list", "--format", "terse"])
                .env_remove(dylib_path_var());
            if let Some(path) = path {
                command.env(dylib_path_var(), path);
            }
            command.output().is_ok_and(|o| o.status.success())
        };
        if cfg!(target_os = "linux") {
            assert!(!lists(None), "needs libstd");
        }
        assert!(lists(std::env::join_paths(dirs).ok()));
    }
}
