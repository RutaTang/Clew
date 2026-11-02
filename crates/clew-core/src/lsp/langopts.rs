//! What there is to choose from for a project's languages in Settings
//! ([`clew_protocol::LanguageOptions`]): the features of its Cargo packages,
//! the build tags its Go files use, its C/C++ compilation databases, the
//! toolchains Lean's and Dart's servers can be run from — beside the Python
//! interpreters [`langenv`] finds. The user picks from these, or types their
//! own; what they pick becomes the server's settings in [`langenv::chosen`].
//!
//! Everything here is found by reading files, never by running anything:
//! listing what a project offers must not be a way to run its code. Every
//! read is capped, and every walk bounded, so a project of any size (or any
//! malice) is listed in bounded time.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clew_protocol::{
    COptions, CargoPackage, GoOptions, LanguageChoices, LanguageOptions, RustOptions,
    ToolchainCandidate,
};
use serde_json::Value;

use super::config::ProjectLspConfig;
use super::langenv;

/// Byte cap for a `Cargo.toml` (or `rust-toolchain.toml`, `.cargo/config.toml`).
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Most workspace members read.
const MAX_MEMBERS: usize = 256;

/// Most `.go` files whose header is read for build tags.
const MAX_GO_FILES: usize = 10_000;

/// Bytes of a `.go` file read for its build constraints, which come before
/// its `package` clause.
const GO_HEADER_BYTES: usize = 4096;

/// Most build tags listed.
const MAX_TAGS: usize = 200;

/// Most entries of a toolchain manager's directory looked at.
const MAX_TOOLCHAINS: usize = 64;

/// What there is to choose from for `languages` (the client's names) in
/// `root`, with the user's `choices` (for the Python interpreter in effect)
/// and the project's `lsp.toml` (`config`, `None` when it cannot be read),
/// whose settings a choice wins over.
pub fn language_options(
    root: &Path,
    languages: &[String],
    choices: &LanguageChoices,
    config: Option<&ProjectLspConfig>,
) -> LanguageOptions {
    let has = |l: &str| languages.iter().any(|x| x == l);
    let options = |language: &str| {
        config
            .and_then(|c| c.resolve(language))
            .and_then(|s| s.init_options)
    };
    let host = ToolHost::current();
    let mut toolchains = BTreeMap::new();
    for language in ["lean", "dart"] {
        if has(language) {
            toolchains.insert(
                language.to_string(),
                toolchain_candidates(language, root, &host),
            );
        }
    }
    LanguageOptions {
        python: (has("python") || choices.python.is_some())
            .then(|| langenv::python_environments(root, choices, options("python").as_ref())),
        rust: has("rust").then(|| RustOptions {
            packages: cargo_packages(root),
            targets: project_targets(root),
            project_sets: project_sets(options("rust").as_ref(), RUST_KEYS),
        }),
        go: has("go").then(|| GoOptions {
            tags: go_build_tags(root),
            project_sets: project_sets(options("go").as_ref(), GO_KEYS),
        }),
        c: (has("c") || has("cpp")).then(|| COptions {
            databases: compile_databases(root),
            project_sets: project_sets(options("c").or_else(|| options("cpp")).as_ref(), C_KEYS),
        }),
        toolchains,
    }
}

/// The rust-analyzer settings a Rust choice sets.
const RUST_KEYS: &[&str] = &[
    "cargo.features",
    "cargo.noDefaultFeatures",
    "cargo.target",
    "check.features",
    "check.targets",
];

/// The gopls settings a Go choice sets.
const GO_KEYS: &[&str] = &["buildFlags", "env.GOOS", "env.GOARCH"];

/// The clangd settings a C/C++ choice sets.
const C_KEYS: &[&str] = &["compilationDatabasePath", "fallbackFlags"];

/// Which of `keys` the project's `lsp.toml` options set — nested
/// (`[rust.init_options.cargo] features = …`), or as one dotted key, with or
/// without the server's prefix (`"rust-analyzer.cargo.features"`).
fn project_sets(options: Option<&Value>, keys: &[&str]) -> Vec<String> {
    let Some(options) = options else {
        return Vec::new();
    };
    keys.iter()
        .filter(|key| {
            let nested = key
                .split('.')
                .try_fold(options, |value, part| value.get(part))
                .is_some();
            nested
                || options.as_object().is_some_and(|map| {
                    map.keys().any(|k| {
                        k == **key || k.split_once('.').is_some_and(|(_, rest)| rest == **key)
                    })
                })
        })
        .map(|key| key.to_string())
        .collect()
}

/// A TOML file of the project's, parsed — `None` when it is missing,
/// oversized, not a plain file, or not TOML.
fn read_toml(path: &Path) -> Option<toml::Table> {
    let text = crate::statefile::read_capped(path, MAX_MANIFEST_BYTES)?;
    text.parse::<toml::Table>().ok()
}

/// The project's Cargo packages: the one at its root, then its workspace's
/// members, each with the features cargo knows it by.
pub fn cargo_packages(root: &Path) -> Vec<CargoPackage> {
    let Some(manifest) = read_toml(&root.join("Cargo.toml")) else {
        return Vec::new();
    };
    let mut packages = Vec::new();
    if let Some(package) = package_of(&manifest, true) {
        packages.push(package);
    }
    let workspace = manifest.get("workspace").and_then(toml::Value::as_table);
    let patterns = |key: &str| -> Vec<String> {
        workspace
            .and_then(|w| w.get(key))
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let excluded: BTreeSet<PathBuf> = patterns("exclude")
        .iter()
        .flat_map(|p| expand_member(root, p))
        .collect();
    let mut seen = BTreeSet::new();
    for pattern in patterns("members") {
        for dir in expand_member(root, &pattern) {
            if packages.len() > MAX_MEMBERS {
                break;
            }
            if excluded.contains(&dir) || dir == root || !seen.insert(dir.clone()) {
                continue;
            }
            if let Some(package) =
                read_toml(&dir.join("Cargo.toml")).and_then(|member| package_of(&member, false))
            {
                packages.push(package);
            }
        }
    }
    let (rooted, mut members): (Vec<_>, Vec<_>) = packages.into_iter().partition(|p| p.root);
    members.sort_by(|a, b| a.name.cmp(&b.name));
    members.dedup_by(|a, b| a.name == b.name);
    rooted.into_iter().chain(members).collect()
}

/// The package `manifest` declares, with its features: its `[features]`
/// table's, and its optional dependencies' — each of which is a feature of
/// its own name unless some feature names it as `dep:name`.
fn package_of(manifest: &toml::Table, root: bool) -> Option<CargoPackage> {
    let name = manifest
        .get("package")?
        .as_table()?
        .get("name")?
        .as_str()?
        .to_string();
    let table = manifest.get("features").and_then(toml::Value::as_table);
    let mut features: BTreeSet<String> = table
        .map(|t| t.keys().filter(|k| *k != "default").cloned().collect())
        .unwrap_or_default();
    let enabled: Vec<&str> = table
        .into_iter()
        .flat_map(|t| t.values())
        .filter_map(toml::Value::as_array)
        .flatten()
        .filter_map(toml::Value::as_str)
        .collect();
    let named_as_dep = |dep: &str| enabled.iter().any(|e| e.strip_prefix("dep:") == Some(dep));
    let mut tables: Vec<&toml::Table> = ["dependencies", "build-dependencies"]
        .iter()
        .filter_map(|k| manifest.get(*k).and_then(toml::Value::as_table))
        .collect();
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for target in targets.values().filter_map(toml::Value::as_table) {
            tables.extend(
                ["dependencies", "build-dependencies"]
                    .iter()
                    .filter_map(|k| target.get(*k).and_then(toml::Value::as_table)),
            );
        }
    }
    for deps in tables {
        for (dep, spec) in deps {
            let optional = spec
                .as_table()
                .and_then(|t| t.get("optional"))
                .and_then(toml::Value::as_bool)
                == Some(true);
            if optional && !named_as_dep(dep) {
                features.insert(dep.clone());
            }
        }
    }
    let default = table
        .and_then(|t| t.get("default"))
        .and_then(toml::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some(CargoPackage {
        name,
        root,
        features: features.into_iter().collect(),
        default,
    })
}

/// The directories a workspace `members` (or `exclude`) entry names: a path,
/// or one with `*` / `?` wildcards in its components, as cargo globs them.
/// Only directories under `root`, found without following a link out of it.
fn expand_member(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    for part in Path::new(pattern).components() {
        let part = match part {
            std::path::Component::Normal(p) => p.to_string_lossy().into_owned(),
            std::path::Component::CurDir => continue,
            // `..` and absolute members leave the project: not listed.
            _ => return Vec::new(),
        };
        let mut next = Vec::new();
        for dir in &dirs {
            if part.contains(['*', '?']) {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                for entry in entries.flatten().take(MAX_MEMBERS * 4) {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if wildcard(&part, &name) && entry.file_type().is_ok_and(|t| t.is_dir()) {
                        next.push(entry.path());
                    }
                }
            } else {
                let path = dir.join(&part);
                if path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                    next.push(path);
                }
            }
        }
        next.sort();
        next.truncate(MAX_MEMBERS);
        dirs = next;
    }
    dirs
}

/// Whether `name` matches the shell wildcard `pattern` (`*`, `?`).
fn wildcard(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    p.len() <= 64 && n.len() <= 256 && go(&p, &n)
}

/// The targets the project names: its `rust-toolchain.toml`'s `targets`,
/// and its `.cargo/config.toml`'s `build.target`.
pub fn project_targets(root: &Path) -> Vec<String> {
    let mut targets = BTreeSet::new();
    let toolchain = read_toml(&root.join("rust-toolchain.toml"))
        .or_else(|| read_toml(&root.join("rust-toolchain")));
    if let Some(list) = toolchain
        .as_ref()
        .and_then(|t| t.get("toolchain"))
        .and_then(|t| t.get("targets"))
        .and_then(toml::Value::as_array)
    {
        targets.extend(
            list.iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_string),
        );
    }
    let config = read_toml(&root.join(".cargo/config.toml"))
        .or_else(|| read_toml(&root.join(".cargo/config")));
    match config
        .as_ref()
        .and_then(|c| c.get("build"))
        .and_then(|b| b.get("target"))
    {
        Some(toml::Value::String(t)) => {
            targets.insert(t.clone());
        }
        Some(toml::Value::Array(list)) => {
            targets.extend(
                list.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_string),
            );
        }
        _ => {}
    }
    targets
        .into_iter()
        .filter(|t| langenv::is_target_triple(t))
        .collect()
}

/// Known `GOOS` values: a build tag that is one is a platform, chosen as
/// such, not a tag.
pub const GOOS: &[&str] = &[
    "aix",
    "android",
    "darwin",
    "dragonfly",
    "freebsd",
    "hurd",
    "illumos",
    "ios",
    "js",
    "linux",
    "nacl",
    "netbsd",
    "openbsd",
    "plan9",
    "solaris",
    "wasip1",
    "windows",
    "zos",
];

/// Known `GOARCH` values.
pub const GOARCH: &[&str] = &[
    "386",
    "amd64",
    "amd64p32",
    "arm",
    "armbe",
    "arm64",
    "arm64be",
    "loong64",
    "mips",
    "mipsle",
    "mips64",
    "mips64le",
    "mips64p32",
    "mips64p32le",
    "ppc",
    "ppc64",
    "ppc64le",
    "riscv",
    "riscv64",
    "s390",
    "s390x",
    "sparc",
    "sparc64",
    "wasm",
];

/// The build tags the project's Go files constrain themselves with — the
/// names in their `//go:build` (and older `// +build`) lines, less the
/// platforms, the compilers, `cgo`, `unix`, `ignore` and Go versions, which
/// are not tags anyone sets. Sorted; the walk skips what `.gitignore` does,
/// `vendor/`, `testdata/` and hidden directories, and reads only each file's
/// header.
pub fn go_build_tags(root: &Path) -> Vec<String> {
    let mut tags = BTreeSet::new();
    let walk = ignore::WalkBuilder::new(root)
        .hidden(true)
        .follow_links(false)
        .filter_entry(|e| {
            !(e.file_type().is_some_and(|t| t.is_dir())
                && matches!(
                    e.file_name().to_str(),
                    Some("vendor" | "testdata" | "node_modules")
                ))
        })
        .build();
    let mut files = 0;
    for entry in walk.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file())
            || entry.path().extension().is_none_or(|e| e != "go")
        {
            continue;
        }
        files += 1;
        if files > MAX_GO_FILES || tags.len() >= MAX_TAGS {
            break;
        }
        let Some(header) = read_head(entry.path(), GO_HEADER_BYTES) else {
            continue;
        };
        for tag in constraint_names(&header) {
            if tags.len() < MAX_TAGS {
                tags.insert(tag);
            }
        }
    }
    tags.into_iter().collect()
}

/// The first `max` bytes of `path`, as text (lossily).
fn read_head(path: &Path, max: usize) -> Option<String> {
    use std::io::Read;
    let file = crate::statefile::open_plain_checked(path).ok()??;
    let mut buf = Vec::with_capacity(max);
    file.take(max as u64).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The tag names a Go file's build constraints use, up to its `package`
/// clause.
fn constraint_names(header: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in header.lines().map(str::trim) {
        if line.starts_with("package ") {
            break;
        }
        let Some(expr) = line
            .strip_prefix("//go:build ")
            .or_else(|| line.strip_prefix("// +build "))
        else {
            continue;
        };
        for word in expr.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.')) {
            if is_settable_tag(word) {
                names.push(word.to_string());
            }
        }
    }
    names
}

/// A constraint name someone would set as a tag.
fn is_settable_tag(word: &str) -> bool {
    let go_version = word
        .strip_prefix("go1.")
        .is_some_and(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()));
    langenv::is_go_tag(word)
        && !go_version
        && !GOOS.contains(&word)
        && !GOARCH.contains(&word)
        && !matches!(word, "cgo" | "gc" | "gccgo" | "unix" | "ignore")
}

/// The project's directories holding a `compile_commands.json`: its root,
/// and where CMake, Meson and their kin put one (`build/`, `build/<type>/`,
/// `cmake-build-<type>/`, `out/build/<preset>/`, `builddir/`, …).
pub fn compile_databases(root: &Path) -> Vec<String> {
    let mut dirs: Vec<PathBuf> = vec![root.to_path_buf()];
    for name in [
        "build", "out", "builddir", ".build", "_build", "debug", "release",
    ] {
        dirs.push(root.join(name));
    }
    for parent in [root.join("build"), root.join("out/build"), root.join("out")] {
        dirs.extend(subdirs(&parent));
    }
    dirs.extend(subdirs(root).into_iter().filter(|d| {
        d.file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("cmake-build-"))
    }));
    let mut seen = BTreeSet::new();
    dirs.into_iter()
        .filter(|d| {
            d.join("compile_commands.json")
                .symlink_metadata()
                .is_ok_and(|m| m.is_file() || m.file_type().is_symlink())
        })
        .filter(|d| seen.insert(d.clone()))
        .take(32)
        .map(|d| d.to_string_lossy().into_owned())
        .collect()
}

/// The directories directly in `dir` (not links), sorted, at most a few
/// dozen.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .take(MAX_TOOLCHAINS * 4)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    out.sort();
    out.truncate(MAX_TOOLCHAINS);
    out
}

/// Where a host keeps its toolchains, as its environment says. Passed in,
/// so a test can point it at a scratch home.
pub struct ToolHost {
    /// The user's home.
    pub home: Option<PathBuf>,
    /// The search path a toolchain's program is looked for on.
    pub path: Option<OsString>,
    /// `ELAN_HOME`, when set: where elan keeps Lean's toolchains.
    pub elan_home: Option<PathBuf>,
    /// `FLUTTER_ROOT`, when set: the Flutter SDK, whose `bin/dart` is Dart's.
    pub flutter_root: Option<PathBuf>,
}

impl ToolHost {
    /// This process's host.
    pub fn current() -> ToolHost {
        let dir = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        ToolHost {
            home: dir("HOME"),
            path: crate::linerpc::spawn_path(),
            elan_home: dir("ELAN_HOME"),
            flutter_root: dir("FLUTTER_ROOT"),
        }
    }
}

/// The toolchains `language`'s server can be run from, on `host`, the one
/// the project names first. None from inside the project: a program the
/// repository ships is not offered to be run.
pub fn toolchain_candidates(
    language: &str,
    root: &Path,
    host: &ToolHost,
) -> Vec<ToolchainCandidate> {
    let mut found = match language {
        "lean" => lean_toolchains(root, host),
        "dart" => dart_sdks(root, host),
        _ => Vec::new(),
    };
    let repo = std::fs::canonicalize(root).ok();
    let mut seen = BTreeSet::new();
    found.retain(|c| {
        let real = std::fs::canonicalize(&c.command).ok();
        let inside = matches!((&real, &repo), (Some(real), Some(repo)) if real.starts_with(repo));
        !inside && seen.insert(c.command.clone())
    });
    // The project's own first, in the order found otherwise.
    found.sort_by_key(|c| !c.project);
    found.truncate(MAX_TOOLCHAINS);
    found
}

/// The executable `dir/binary`, when there is one, as a candidate.
fn candidate(path: PathBuf, name: String, project: bool) -> Option<ToolchainCandidate> {
    crate::lsp::store::find_program(&path.to_string_lossy())
        .ok()
        .map(|path| ToolchainCandidate {
            command: path.to_string_lossy().into_owned(),
            name,
            project,
        })
}

/// Lean's `lake`: elan's own (which runs the toolchain the project's
/// `lean-toolchain` names), each toolchain elan has installed, and one on
/// `PATH`.
fn lean_toolchains(root: &Path, host: &ToolHost) -> Vec<ToolchainCandidate> {
    let mut out = Vec::new();
    let wanted = crate::statefile::read_capped(&root.join("lean-toolchain"), 4096)
        .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
        .filter(|t| !t.is_empty());
    let elan = host
        .elan_home
        .clone()
        .or_else(|| host.home.as_ref().map(|h| h.join(".elan")));
    if let Some(elan) = &elan {
        out.extend(candidate(
            elan.join("bin/lake"),
            "elan — the toolchain the project's lean-toolchain names".into(),
            wanted.is_some(),
        ));
        for dir in subdirs(&elan.join("toolchains")) {
            let folder = dir.file_name().unwrap_or_default().to_string_lossy();
            // elan spells `leanprover/lean4:v4.9.0` as `leanprover--lean4---v4.9.0`.
            let name = folder.replace("---", ":").replace("--", "/");
            let project = wanted.as_deref() == Some(name.as_str());
            out.extend(candidate(dir.join("bin/lake"), name, project));
        }
    }
    if let Some(path) = host.path.as_deref()
        && let Some(lake) = crate::lsp::store::find_in_unresolved(path, "lake")
    {
        out.extend(candidate(lake, "lake on PATH".into(), false));
    }
    out
}

/// Dart's `dart`: one on `PATH`, the Flutter SDK's (`FLUTTER_ROOT`, and
/// where it is usually unpacked), the system's, and each Flutter version
/// fvm keeps — the one the project's `.fvmrc` names marked.
fn dart_sdks(root: &Path, host: &ToolHost) -> Vec<ToolchainCandidate> {
    let mut out = Vec::new();
    if let Some(path) = host.path.as_deref()
        && let Some(dart) = crate::lsp::store::find_in_unresolved(path, "dart")
    {
        out.extend(candidate(dart, "dart on PATH".into(), false));
    }
    if let Some(flutter) = &host.flutter_root {
        out.extend(candidate(
            flutter.join("bin/dart"),
            "Flutter (FLUTTER_ROOT)".into(),
            false,
        ));
    }
    if let Some(home) = &host.home {
        for sdk in [
            "flutter",
            "development/flutter",
            "sdk/flutter",
            "dev/flutter",
        ] {
            out.extend(candidate(
                home.join(sdk).join("bin/dart"),
                format!("Flutter (~/{sdk})"),
                false,
            ));
        }
        let pinned = fvm_version(root);
        for dir in subdirs(&home.join("fvm/versions")) {
            let version = dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let project = pinned.as_deref() == Some(version.as_str());
            out.extend(candidate(
                dir.join("bin/dart"),
                format!("fvm · Flutter {version}"),
                project,
            ));
        }
    }
    for (path, name) in [
        ("/opt/flutter/bin/dart", "Flutter (/opt/flutter)"),
        ("/usr/lib/dart/bin/dart", "Dart SDK (/usr/lib/dart)"),
        ("/opt/homebrew/bin/dart", "Homebrew"),
        ("/usr/local/bin/dart", "/usr/local/bin"),
    ] {
        out.extend(candidate(PathBuf::from(path), name.into(), false));
    }
    out
}

/// The Flutter version the project pins with fvm (`.fvmrc`, or the older
/// `.fvm/fvm_config.json`).
fn fvm_version(root: &Path) -> Option<String> {
    let read = |file: &str, key: &str| {
        let text = crate::statefile::read_capped(&root.join(file), 64 * 1024)?;
        let json: Value = serde_json::from_str(&text).ok()?;
        json.get(key)?.as_str().map(str::to_string)
    };
    read(".fvmrc", "flutter").or_else(|| read(".fvm/fvm_config.json", "flutterSdkVersion"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A workspace's packages and features, as cargo names them: the root
    /// package first, then its members (globbed, excluded ones left out),
    /// each with its `[features]` and the optional dependencies that are
    /// features of their own.
    #[test]
    fn a_workspace_lists_its_packages_and_their_features() {
        let dir = TempDir::new("langopts-cargo");
        let root = dir.path();
        write(
            &root.join("Cargo.toml"),
            r#"
[package]
name = "app"
[features]
default = ["tls"]
tls = ["dep:rustls"]
cli = []
[dependencies]
rustls = { version = "1", optional = true }
serde = { version = "1", optional = true }
log = "0.4"
[target.'cfg(unix)'.dependencies]
nix = { version = "0.29", optional = true }
[workspace]
members = ["crates/*", "tools/gen"]
exclude = ["crates/skip"]
"#,
        );
        write(
            &root.join("crates/core/Cargo.toml"),
            "[package]\nname = \"core\"\n[features]\nfast = []\n",
        );
        write(
            &root.join("crates/skip/Cargo.toml"),
            "[package]\nname = \"skip\"\n",
        );
        write(
            &root.join("tools/gen/Cargo.toml"),
            "[package]\nname = \"gen\"\n",
        );
        let packages = cargo_packages(root);
        let names: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["app", "core", "gen"]);
        assert!(packages[0].root && !packages[1].root);
        // `rustls` is named as `dep:rustls`, so it is no feature of its own.
        assert_eq!(packages[0].features, ["cli", "nix", "serde", "tls"]);
        assert_eq!(packages[0].default, ["tls"]);
        assert_eq!(packages[1].features, ["fast"]);
    }

    /// A member that leaves the project is not read.
    #[test]
    fn a_member_outside_the_project_is_not_read() {
        let dir = TempDir::new("langopts-escape");
        let root = dir.path().join("p");
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"../outside\", \"/abs\"]\n",
        );
        write(
            &dir.path().join("outside/Cargo.toml"),
            "[package]\nname = \"outside\"\n",
        );
        assert!(cargo_packages(&root).is_empty());
    }

    /// The targets a project names, from both files that name them.
    #[test]
    fn the_projects_targets_are_read_from_its_toolchain_and_cargo_config() {
        let dir = TempDir::new("langopts-targets");
        let root = dir.path();
        write(
            &root.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\ntargets = [\"wasm32-unknown-unknown\", \"bad target\"]\n",
        );
        write(
            &root.join(".cargo/config.toml"),
            "[build]\ntarget = \"thumbv7em-none-eabihf\"\n",
        );
        assert_eq!(
            project_targets(root),
            ["thumbv7em-none-eabihf", "wasm32-unknown-unknown"]
        );
    }

    /// The tags a project's Go files use: from either constraint syntax, up
    /// to the package clause, without platforms, versions or `cgo`, and not
    /// from vendored or test-data files.
    #[test]
    fn go_build_tags_are_the_ones_the_files_use() {
        let dir = TempDir::new("langopts-go");
        let root = dir.path();
        write(
            &root.join("a.go"),
            "//go:build integration && !windows && go1.21\n\npackage a\n//go:build late\n",
        );
        write(
            &root.join("b/b.go"),
            "// +build e2e,linux cgo\n\npackage b\n",
        );
        write(
            &root.join("vendor/v/v.go"),
            "//go:build vendored\npackage v\n",
        );
        write(
            &root.join("testdata/t.go"),
            "//go:build fixture\npackage t\n",
        );
        write(&root.join("c.txt"), "//go:build notgo\n");
        assert_eq!(go_build_tags(root), ["e2e", "integration"]);
    }

    /// Compilation databases where build systems put them, the root's first.
    #[test]
    fn compile_databases_are_found_where_build_systems_put_them() {
        let dir = TempDir::new("langopts-ccdb");
        let root = dir.path();
        for at in ["", "build/debug", "cmake-build-release", "out/build/x64"] {
            write(&root.join(at).join("compile_commands.json"), "[]");
        }
        write(&root.join("elsewhere/compile_commands.json"), "[]");
        let found: Vec<String> = compile_databases(root)
            .into_iter()
            .map(|d| {
                Path::new(&d)
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            found,
            ["", "build/debug", "out/build/x64", "cmake-build-release"]
        );
    }

    #[cfg(unix)]
    fn executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        write(path, "#!/bin/sh\n");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Lean's toolchains under elan, the one the project's `lean-toolchain`
    /// names first, and none from inside the project.
    #[cfg(unix)]
    #[test]
    fn lean_toolchains_are_found_under_elan_the_projects_first() {
        let dir = TempDir::new("langopts-lean");
        let home = dir.path().join("home");
        let root = dir.path().join("proj");
        write(&root.join("lean-toolchain"), "leanprover/lean4:v4.9.0\n");
        executable(&home.join(".elan/bin/lake"));
        executable(&home.join(".elan/toolchains/leanprover--lean4---v4.8.0/bin/lake"));
        executable(&home.join(".elan/toolchains/leanprover--lean4---v4.9.0/bin/lake"));
        // A `lake` the repository ships, on PATH: not offered.
        executable(&root.join("bin/lake"));
        let host = ToolHost {
            home: Some(home.clone()),
            path: Some(root.join("bin").into_os_string()),
            elan_home: None,
            flutter_root: None,
        };
        let found = toolchain_candidates("lean", &root, &host);
        let names: Vec<(&str, bool)> = found.iter().map(|c| (c.name.as_str(), c.project)).collect();
        assert_eq!(
            names,
            [
                (
                    "elan — the toolchain the project's lean-toolchain names",
                    true
                ),
                ("leanprover/lean4:v4.9.0", true),
                ("leanprover/lean4:v4.8.0", false),
            ]
        );
        assert!(found[0].command.ends_with(".elan/bin/lake"));
    }

    /// Dart SDKs: fvm's versions, the project's pinned one first.
    #[cfg(unix)]
    #[test]
    fn dart_sdks_include_fvm_versions_the_projects_first() {
        let dir = TempDir::new("langopts-dart");
        let home = dir.path().join("home");
        let root = dir.path().join("proj");
        write(&root.join(".fvmrc"), r#"{"flutter": "3.22.0"}"#);
        executable(&home.join("flutter/bin/dart"));
        executable(&home.join("fvm/versions/3.19.0/bin/dart"));
        executable(&home.join("fvm/versions/3.22.0/bin/dart"));
        let host = ToolHost {
            home: Some(home),
            path: None,
            elan_home: None,
            flutter_root: None,
        };
        let names: Vec<String> = toolchain_candidates("dart", &root, &host)
            .into_iter()
            .filter(|c| !c.command.starts_with("/opt") && !c.command.starts_with("/usr"))
            .map(|c| c.name)
            .collect();
        assert_eq!(
            names,
            [
                "fvm · Flutter 3.22.0",
                "Flutter (~/flutter)",
                "fvm · Flutter 3.19.0"
            ]
        );
    }

    /// What the project's `lsp.toml` already sets of a language's choices,
    /// in any of the spellings a server reads.
    #[test]
    fn the_projects_own_settings_are_named() {
        let options = serde_json::json!({
            "cargo": { "features": ["x"] },
            "rust-analyzer.check.targets": ["wasm32-unknown-unknown"],
        });
        assert_eq!(
            project_sets(Some(&options), RUST_KEYS),
            ["cargo.features", "check.targets"]
        );
        assert!(project_sets(None, RUST_KEYS).is_empty());
    }
}
