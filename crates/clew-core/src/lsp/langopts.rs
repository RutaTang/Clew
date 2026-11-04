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
    for language in ["lean", "dart", "zig", "java", "ruby", "erlang", "haskell"] {
        if has(language) {
            toolchains.insert(
                language.to_string(),
                toolchain_candidates(language, root, &host),
            );
        }
    }
    if has("typescript") || has("tsx") || has("javascript") {
        toolchains.insert("typescript".into(), typescript_installs(root, &host));
    }
    if has("php") {
        toolchains.insert("php".into(), php_versions(root));
    }
    if has("csharp") {
        toolchains.insert("csharp".into(), solutions(root));
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

/// Which of `keys` the project's `lsp.toml` options set ([`setting`]).
fn project_sets(options: Option<&Value>, keys: &[&str]) -> Vec<String> {
    let Some(options) = options else {
        return Vec::new();
    };
    keys.iter()
        .filter(|key| setting(options, key).is_some())
        .map(|key| key.to_string())
        .collect()
}

/// The value server `options` give the dotted `key` — nested
/// (`[rust.init_options.cargo] features = …`), or as one dotted key, with or
/// without the server's prefix (`"rust-analyzer.cargo.features"`).
pub fn setting<'a>(options: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.')
        .try_fold(options, |value, part| value.get(part))
        .or_else(|| {
            options.as_object()?.iter().find_map(|(k, v)| {
                (k == key || k.split_once('.').is_some_and(|(_, rest)| rest == key)).then_some(v)
            })
        })
}

/// A TOML file of the project's, parsed — `None` when it is missing,
/// oversized, not a plain file, or not TOML.
pub(crate) fn read_toml(path: &Path) -> Option<toml::Table> {
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
    for dir in workspace_members(root, &manifest) {
        if let Some(package) =
            read_toml(&dir.join("Cargo.toml")).and_then(|member| package_of(&member, false))
        {
            packages.push(package);
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
pub(crate) fn expand_member(root: &Path, pattern: &str) -> Vec<PathBuf> {
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

/// The members a workspace manifest names — `members` globbed, `exclude`
/// left out, the root itself not among them — each once.
pub(crate) fn workspace_members(root: &Path, manifest: &toml::Table) -> Vec<PathBuf> {
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
    let mut members = Vec::new();
    for pattern in patterns("members") {
        for dir in expand_member(root, &pattern) {
            if members.len() >= MAX_MEMBERS {
                return members;
            }
            if !excluded.contains(&dir) && dir != root && seen.insert(dir.clone()) {
                members.push(dir);
            }
        }
    }
    members
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
    for parent in [
        root.join("build"),
        root.join("out").join("build"),
        root.join("out"),
    ] {
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

/// The directories in `dir` whose names `keep` keeps, links to directories
/// among them — where a host keeps its JDKs, links name them (SDKMAN!'s
/// `current`, Debian's `default-java`, Homebrew's `opt/openjdk`) — sorted,
/// at most a few dozen. Only for a host's own folders: a project's are read
/// with [`subdirs`], which follows no link out of it.
fn linked_subdirs(dir: &Path, keep: fn(&str) -> bool) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| keep(&e.file_name().to_string_lossy()))
        .take(MAX_TOOLCHAINS * 4)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    out.sort();
    out.truncate(MAX_TOOLCHAINS);
    out
}

/// Where a host keeps its toolchains, as its environment says. Passed in,
/// so a test can point it at a scratch home.
#[derive(Default)]
pub struct ToolHost {
    /// The user's home.
    pub home: Option<PathBuf>,
    /// The search path a toolchain's program is looked for on.
    pub path: Option<OsString>,
    /// `ELAN_HOME`, when set: where elan keeps Lean's toolchains.
    pub elan_home: Option<PathBuf>,
    /// `FLUTTER_ROOT`, when set: the Flutter SDK, whose `bin/dart` is Dart's.
    pub flutter_root: Option<PathBuf>,
    /// `JAVA_HOME`, when set.
    pub java_home: Option<PathBuf>,
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
            java_home: dir("JAVA_HOME"),
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
        "zig" => zig_compilers(root, host),
        "java" => jdks(root, host),
        "ruby" => ruby_lsps(root, host),
        "erlang" => programs(
            host,
            "elp",
            &["/opt/homebrew/bin", "/usr/local/bin"],
            &[".local/bin"],
        ),
        "haskell" => programs(
            host,
            "haskell-language-server-wrapper",
            &["/opt/homebrew/bin", "/usr/local/bin"],
            &[".ghcup/bin", ".local/bin", ".cabal/bin"],
        ),
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

/// Zig's `zig`: one on `PATH`, zvm's (its current one and each version it
/// keeps), zigup's, and where package managers put one — the version the
/// project's `build.zig.zon` asks for (`minimum_zig_version`) marked.
fn zig_compilers(root: &Path, host: &ToolHost) -> Vec<ToolchainCandidate> {
    let wanted =
        crate::statefile::read_capped(&root.join("build.zig.zon"), 64 * 1024).and_then(|text| {
            let at = text.find(".minimum_zig_version")?;
            let rest = &text[at..];
            let start = rest.find('"')? + 1;
            let len = rest[start..].find('"')?;
            Some(rest[start..start + len].to_string())
        });
    let names_wanted = |name: &str| wanted.as_deref().is_some_and(|w| name.contains(w));
    let mut out = Vec::new();
    if let Some(path) = host.path.as_deref()
        && let Some(zig) = crate::lsp::store::find_in_unresolved(path, "zig")
    {
        out.extend(candidate(zig, "zig on PATH".into(), false));
    }
    if let Some(home) = &host.home {
        out.extend(candidate(
            home.join(".zvm/bin/zig"),
            "zvm (its current one)".into(),
            false,
        ));
        for dir in subdirs(&home.join(".zvm")) {
            let version = dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if version == "bin" || version == "self" {
                continue;
            }
            let project = names_wanted(&version);
            out.extend(candidate(
                dir.join("zig"),
                format!("zvm · Zig {version}"),
                project,
            ));
        }
        for dir in subdirs(&home.join("zig")) {
            let version = dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let project = names_wanted(&version);
            out.extend(candidate(
                dir.join("files/zig"),
                format!("zigup · Zig {version}"),
                project,
            ));
        }
    }
    for (path, name) in [
        ("/opt/homebrew/bin/zig", "Homebrew"),
        ("/usr/local/bin/zig", "/usr/local/bin"),
        ("/snap/bin/zig", "snap"),
    ] {
        out.extend(candidate(PathBuf::from(path), name.into(), false));
    }
    out
}

/// A program on `PATH`, in `dirs`, or in `home_dirs` under the home — a
/// language server installed where its installers put it.
fn programs(
    host: &ToolHost,
    binary: &str,
    dirs: &[&str],
    home_dirs: &[&str],
) -> Vec<ToolchainCandidate> {
    let mut out = Vec::new();
    if let Some(path) = host.path.as_deref()
        && let Some(found) = crate::lsp::store::find_in_unresolved(path, binary)
    {
        out.extend(candidate(found, format!("{binary} on PATH"), false));
    }
    if let Some(home) = &host.home {
        for dir in home_dirs {
            out.extend(candidate(
                home.join(dir).join(binary),
                format!("~/{dir}"),
                false,
            ));
        }
    }
    for dir in dirs {
        out.extend(candidate(
            Path::new(dir).join(binary),
            dir.to_string(),
            false,
        ));
    }
    out
}

/// The JDKs on `host`: `JAVA_HOME`'s, macOS's `JavaVirtualMachines`, Linux's
/// `/usr/lib/jvm`, and those SDKMAN!, IntelliJ, asdf, mise and Homebrew
/// keep — each once, named by its vendor and version (its `release` file),
/// the ones of the Java version the project's build asks for marked.
fn jdks(root: &Path, host: &ToolHost) -> Vec<ToolchainCandidate> {
    let wanted = project_java(root);
    let mut homes: Vec<PathBuf> = Vec::new();
    homes.extend(host.java_home.clone());
    let mut each_named = |dir: PathBuf, keep: fn(&str) -> bool, inner: &str| {
        for jdk in linked_subdirs(&dir, keep) {
            homes.push(if inner.is_empty() {
                jdk
            } else {
                jdk.join(inner)
            });
        }
    };
    let all: fn(&str) -> bool = |_| true;
    each_named(
        PathBuf::from("/Library/Java/JavaVirtualMachines"),
        all,
        "Contents/Home",
    );
    each_named(PathBuf::from("/usr/lib/jvm"), all, "");
    if let Some(home) = &host.home {
        each_named(
            home.join("Library/Java/JavaVirtualMachines"),
            all,
            "Contents/Home",
        );
        each_named(home.join(".sdkman/candidates/java"), all, "");
        each_named(home.join(".jdks"), all, "");
        each_named(home.join(".asdf/installs/java"), all, "");
        each_named(home.join(".local/share/mise/installs/java"), all, "");
    }
    // Homebrew's `opt` holds a link per formula, its JDKs `openjdk` and
    // `openjdk@<n>`.
    for brew in ["/opt/homebrew/opt", "/usr/local/opt"] {
        each_named(
            PathBuf::from(brew),
            |name| name.starts_with("openjdk"),
            "libexec/openjdk.jdk/Contents/Home",
        );
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for home in homes {
        if !home.join("bin/java").is_file() && !home.join("bin/java.exe").is_file() {
            continue;
        }
        let Ok(real) = std::fs::canonicalize(&home) else {
            continue;
        };
        if !seen.insert(real) {
            continue;
        }
        let version = jdk_version(&home);
        let vendor = jdk_release(&home, "IMPLEMENTOR");
        let name = match (&vendor, &version) {
            (Some(vendor), Some(version)) => format!("{vendor} {version}"),
            (None, Some(version)) => format!("JDK {version}"),
            _ => home
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "JDK".into()),
        };
        let project = matches!(
            (wanted, version.as_deref().and_then(java_major)),
            (Some(w), Some(m)) if w == m
        );
        out.push(ToolchainCandidate {
            command: home.to_string_lossy().into_owned(),
            name,
            project,
        });
    }
    out
}

/// A value of a JDK's `release` file (`JAVA_VERSION="21.0.1"`).
fn jdk_release(home: &Path, key: &str) -> Option<String> {
    let text = crate::statefile::read_capped(&home.join("release"), 64 * 1024)?;
    text.lines().find_map(|line| {
        let value = line.strip_prefix(key)?.strip_prefix('=')?;
        Some(value.trim().trim_matches('"').to_string())
    })
}

/// A JDK's version, from its `release` file.
pub fn jdk_version(home: &Path) -> Option<String> {
    jdk_release(home, "JAVA_VERSION")
}

/// A Java version's major number: `21.0.1` → 21, `1.8.0_392` → 8, `17` → 17.
pub fn java_major(version: &str) -> Option<u32> {
    let mut parts = version.trim().split(['.', '_', '-', '+']);
    match parts.next()?.parse().ok()? {
        1 => parts.next()?.parse().ok(),
        major => Some(major),
    }
}

/// The Java version the project's build asks for: `.java-version`,
/// `.sdkmanrc`, `.tool-versions`, a Gradle toolchain
/// (`JavaLanguageVersion.of(17)`), or Maven's `maven.compiler.release` /
/// `java.version`.
fn project_java(root: &Path) -> Option<u32> {
    let read = |file: &str| crate::statefile::read_capped(&root.join(file), 256 * 1024);
    if let Some(text) = read(".java-version") {
        return java_major(text.lines().next()?);
    }
    for (file, key) in [(".sdkmanrc", "java="), (".tool-versions", "java ")] {
        if let Some(text) = read(file)
            && let Some(version) = text.lines().find_map(|l| l.trim().strip_prefix(key))
        {
            let version = version
                .trim()
                .trim_start_matches(|c: char| !c.is_ascii_digit());
            return java_major(version);
        }
    }
    for file in ["build.gradle.kts", "build.gradle"] {
        if let Some(text) = read(file)
            && let Some(at) = text.find("JavaLanguageVersion.of(")
        {
            let rest = &text[at + "JavaLanguageVersion.of(".len()..];
            let number: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            return number.parse().ok();
        }
    }
    let pom = read("pom.xml")?;
    for tag in [
        "maven.compiler.release",
        "maven.compiler.source",
        "java.version",
        "release",
    ] {
        let open = format!("<{tag}>");
        if let Some(at) = pom.find(&open) {
            let rest = &pom[at + open.len()..];
            let value: String = rest.chars().take_while(|c| *c != '<').collect();
            if let Some(major) = java_major(&value) {
                return Some(major);
            }
        }
    }
    None
}

/// The PHP versions intelephense can read code as — the one the project's
/// `composer.json` asks for (`"require": {"php": "^8.1"}`: its lowest) marked
/// and first.
fn php_versions(root: &Path) -> Vec<ToolchainCandidate> {
    let wanted = crate::statefile::read_capped(&root.join("composer.json"), 1024 * 1024)
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|json| {
            json.get("require")?
                .get("php")?
                .as_str()
                .map(str::to_string)
        })
        .and_then(|constraint| {
            // The lowest `major.minor` the constraint names.
            let at = constraint.find(|c: char| c.is_ascii_digit())?;
            let mut parts = constraint[at..].split(|c: char| !c.is_ascii_digit());
            Some(format!("{}.{}", parts.next()?, parts.next().unwrap_or("0")))
        });
    let mut versions: Vec<String> = ["8.4", "8.3", "8.2", "8.1", "8.0", "7.4", "7.3"]
        .iter()
        .map(|v| v.to_string())
        .collect();
    if let Some(w) = &wanted
        && !versions.contains(w)
    {
        versions.push(w.clone());
    }
    let mut out: Vec<ToolchainCandidate> = versions
        .into_iter()
        .map(|v| ToolchainCandidate {
            project: wanted.as_deref() == Some(v.as_str()),
            name: format!("PHP {v}"),
            command: v,
        })
        .collect();
    out.sort_by_key(|c| !c.project);
    out
}

/// The project's C# solutions: `.sln` and `.slnx` files at its root and a
/// folder down.
fn solutions(root: &Path) -> Vec<ToolchainCandidate> {
    let mut dirs = vec![root.to_path_buf()];
    dirs.extend(subdirs(root));
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        // Solutions only, then capped: a folder of many files may list its
        // solution last.
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "sln" || e == "slnx"))
            .filter(|p| p.is_file())
            .take(MAX_TOOLCHAINS)
            .collect();
        found.sort();
        for path in found {
            let name = path
                .strip_prefix(root)
                .map(crate::rel_string)
                .unwrap_or_default();
            out.push(ToolchainCandidate {
                command: path.to_string_lossy().into_owned(),
                name,
                project: true,
            });
        }
    }
    out.truncate(MAX_TOOLCHAINS);
    out
}

/// Ruby LSP as each installed Ruby has it — rbenv's, chruby's, asdf's,
/// mise's, RVM's, Homebrew's and the user's gem directory — the Ruby the
/// project's `.ruby-version` (or `.tool-versions`) names marked; and one on
/// `PATH`.
fn ruby_lsps(root: &Path, host: &ToolHost) -> Vec<ToolchainCandidate> {
    let read = |file: &str| crate::statefile::read_capped(&root.join(file), 4096);
    let wanted = read(".ruby-version")
        .and_then(|t| {
            t.lines()
                .next()
                .map(|l| l.trim().trim_start_matches("ruby-").to_string())
        })
        .or_else(|| {
            read(".tool-versions")?
                .lines()
                .find_map(|l| l.trim().strip_prefix("ruby ").map(|v| v.trim().to_string()))
        })
        .filter(|w| !w.is_empty());
    let mut out = Vec::new();
    if let Some(path) = host.path.as_deref()
        && let Some(found) = crate::lsp::store::find_in_unresolved(path, "ruby-lsp")
    {
        out.extend(candidate(found, "ruby-lsp on PATH".into(), false));
    }
    if let Some(home) = &host.home {
        for (manager, dir, bin) in [
            ("rbenv", ".rbenv/versions", "bin"),
            ("chruby", ".rubies", "bin"),
            ("asdf", ".asdf/installs/ruby", "bin"),
            ("mise", ".local/share/mise/installs/ruby", "bin"),
            ("RVM", ".rvm/gems", "bin"),
            ("gem --user-install", ".gem/ruby", "bin"),
        ] {
            for install in subdirs(&home.join(dir)) {
                let version = install
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let plain = version.trim_start_matches("ruby-");
                let project = wanted.as_deref() == Some(plain);
                out.extend(candidate(
                    install.join(bin).join("ruby-lsp"),
                    format!("{manager} · Ruby {plain}"),
                    project,
                ));
            }
        }
    }
    for dir in subdirs(Path::new("/opt/homebrew/lib/ruby/gems")) {
        let version = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        out.extend(candidate(
            dir.join("bin/ruby-lsp"),
            format!("Homebrew · Ruby {version}"),
            false,
        ));
    }
    out
}

/// The TypeScripts the TypeScript server can run, as `tsserver.js` files:
/// the project's own `node_modules/typescript` — marked as the project's,
/// for running it runs the project's code — and the global installs npm,
/// Homebrew and Volta leave. clew's own, the default, is not listed.
pub fn typescript_installs(root: &Path, host: &ToolHost) -> Vec<ToolchainCandidate> {
    let version = |package: &Path| {
        let text = crate::statefile::read_capped(&package.join("package.json"), 256 * 1024)?;
        let json: Value = serde_json::from_str(&text).ok()?;
        json.get("version")?.as_str().map(str::to_string)
    };
    let install = |package: PathBuf, label: &dyn Fn(String) -> String, project: bool| {
        let tsserver = package.join("lib/tsserver.js");
        tsserver.is_file().then(|| ToolchainCandidate {
            command: tsserver.to_string_lossy().into_owned(),
            name: label(version(&package).unwrap_or_else(|| "(version unknown)".into())),
            project,
        })
    };
    let mut out: Vec<ToolchainCandidate> = Vec::new();
    out.extend(install(
        root.join("node_modules/typescript"),
        &|v| format!("The project's TypeScript {v} — runs the project's own code"),
        true,
    ));
    let mut globals = vec![
        PathBuf::from("/usr/local/lib/node_modules/typescript"),
        PathBuf::from("/opt/homebrew/lib/node_modules/typescript"),
        PathBuf::from("/usr/lib/node_modules/typescript"),
    ];
    if let Some(home) = &host.home {
        globals.push(home.join(".npm-global/lib/node_modules/typescript"));
        globals
            .push(home.join(".volta/tools/image/packages/typescript/lib/node_modules/typescript"));
    }
    for package in globals {
        let place = package.to_string_lossy().into_owned();
        out.extend(install(
            package,
            &|v| format!("TypeScript {v} (installed in {place})"),
            false,
        ));
    }
    let mut seen = BTreeSet::new();
    out.retain(|c| seen.insert(c.command.clone()));
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
                    .replace('\\', "/")
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
            java_home: None,
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
            java_home: None,
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

    /// The project's own TypeScript is offered — named as running the
    /// project's code — with its version, beside global installs.
    #[test]
    fn the_projects_typescript_is_offered_as_the_projects() {
        let dir = TempDir::new("langopts-ts");
        let root = dir.path().join("proj");
        let package = root.join("node_modules/typescript");
        write(&package.join("lib/tsserver.js"), "// tsserver");
        write(
            &package.join("package.json"),
            r#"{"name": "typescript", "version": "5.4.5"}"#,
        );
        let host = ToolHost {
            home: Some(dir.path().join("home")),
            path: None,
            elan_home: None,
            flutter_root: None,
            java_home: None,
        };
        let found = typescript_installs(&root, &host);
        assert_eq!(
            found[0].name,
            "The project's TypeScript 5.4.5 — runs the project's own code"
        );
        assert!(found[0].project);
        assert_eq!(
            found[0].command,
            package.join("lib/tsserver.js").to_string_lossy()
        );
    }

    /// Zig compilers under zvm and zigup, the version the project's
    /// `build.zig.zon` asks for marked and first.
    #[cfg(unix)]
    #[test]
    fn zig_compilers_are_found_the_projects_version_first() {
        let dir = TempDir::new("langopts-zig");
        let home = dir.path().join("home");
        let root = dir.path().join("proj");
        write(
            &root.join("build.zig.zon"),
            ".{ .name = \"app\", .minimum_zig_version = \"0.13.0\" }",
        );
        executable(&home.join(".zvm/bin/zig"));
        executable(&home.join(".zvm/0.12.0/zig"));
        executable(&home.join(".zvm/0.13.0/zig"));
        executable(&home.join("zig/0.11.0/files/zig"));
        let host = ToolHost {
            home: Some(home),
            path: None,
            elan_home: None,
            flutter_root: None,
            java_home: None,
        };
        let names: Vec<(String, bool)> = toolchain_candidates("zig", &root, &host)
            .into_iter()
            .filter(|c| {
                !c.command.starts_with("/opt")
                    && !c.command.starts_with("/usr")
                    && !c.command.starts_with("/snap")
            })
            .map(|c| (c.name, c.project))
            .collect();
        assert_eq!(
            names,
            [
                ("zvm · Zig 0.13.0".to_string(), true),
                ("zvm (its current one)".to_string(), false),
                ("zvm · Zig 0.12.0".to_string(), false),
                ("zigup · Zig 0.11.0".to_string(), false),
            ]
        );
    }

    /// JDKs where SDKMAN! and IntelliJ keep them, each once, named by vendor
    /// and version — those of the Java version the project's build asks for
    /// first.
    #[cfg(unix)]
    #[test]
    fn jdks_are_found_the_projects_version_first() {
        let dir = TempDir::new("langopts-jdks");
        let home = dir.path().join("home");
        let root = dir.path().join("proj");
        write(
            &root.join("build.gradle.kts"),
            "java { toolchain { languageVersion = JavaLanguageVersion.of(17) } }",
        );
        for (at, version) in [
            (".sdkman/candidates/java/21.0.1-tem", "21.0.1"),
            (".jdks/corretto-17.0.9", "17.0.9"),
        ] {
            let jdk = home.join(at);
            executable(&jdk.join("bin/java"));
            write(
                &jdk.join("release"),
                &format!("IMPLEMENTOR=\"Vendor\"\nJAVA_VERSION=\"{version}\"\n"),
            );
        }
        // The same JDK by another name is listed once.
        std::os::unix::fs::symlink(
            home.join(".jdks/corretto-17.0.9"),
            home.join(".sdkman/candidates/java/current"),
        )
        .unwrap();
        // One only a link names (as Homebrew's are) is listed.
        let elsewhere = dir.path().join("elsewhere/jdk-11");
        executable(&elsewhere.join("bin/java"));
        write(
            &elsewhere.join("release"),
            "IMPLEMENTOR=\"Vendor\"\nJAVA_VERSION=\"11.0.2\"\n",
        );
        std::os::unix::fs::symlink(&elsewhere, home.join(".jdks/linked-11")).unwrap();
        let host = ToolHost {
            java_home: Some(home.join(".sdkman/candidates/java/current")),
            home: Some(home),
            ..Default::default()
        };
        let found: Vec<(String, bool)> = toolchain_candidates("java", &root, &host)
            .into_iter()
            // The scratch JDKs only: the host's own (a CI runner's
            // Homebrew JDK) are found too.
            .filter(|c| Path::new(&c.command).starts_with(dir.path()))
            .map(|c| (c.name, c.project))
            .collect();
        assert_eq!(
            found,
            [
                ("Vendor 17.0.9".to_string(), true),
                ("Vendor 21.0.1".to_string(), false),
                ("Vendor 11.0.2".to_string(), false)
            ]
        );
        assert_eq!(java_major("1.8.0_392"), Some(8));
        assert_eq!(java_major("21"), Some(21));
    }

    /// PHP versions, the project's `composer.json` floor first; the
    /// project's C# solutions; Ruby LSP in the Ruby `.ruby-version` names;
    /// ghcup's Haskell language server.
    #[cfg(unix)]
    #[test]
    fn php_versions_solutions_rubies_and_hls_are_listed() {
        let dir = TempDir::new("langopts-more");
        let home = dir.path().join("home");
        let root = dir.path().join("proj");
        write(
            &root.join("composer.json"),
            r#"{"require": {"php": "^8.1 || ^9"}}"#,
        );
        let php = php_versions(&root);
        assert_eq!((php[0].command.as_str(), php[0].project), ("8.1", true));
        assert!(php.iter().filter(|c| c.project).count() == 1);

        write(&root.join("Shop.sln"), "");
        write(&root.join("tools/Tools.slnx"), "");
        // A solution among many files is found all the same.
        for i in 0..MAX_TOOLCHAINS * 5 {
            write(&root.join(format!("src/f{i}.cs")), "");
        }
        write(&root.join("src/App.sln"), "");
        std::fs::create_dir_all(root.join("src/Not.sln")).unwrap();
        let names: Vec<String> = solutions(&root).into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["Shop.sln", "src/App.sln", "tools/Tools.slnx"]);

        write(&root.join(".ruby-version"), "ruby-3.3.0\n");
        executable(&home.join(".rbenv/versions/3.2.2/bin/ruby-lsp"));
        executable(&home.join(".rbenv/versions/3.3.0/bin/ruby-lsp"));
        executable(&home.join(".rubies/ruby-3.3.0/bin/ruby-lsp"));
        executable(&home.join(".ghcup/bin/haskell-language-server-wrapper"));
        let host = ToolHost {
            home: Some(home),
            ..Default::default()
        };
        let rubies: Vec<(String, bool)> = toolchain_candidates("ruby", &root, &host)
            .into_iter()
            .filter(|c| c.command.contains("rbenv") || c.command.contains("rubies"))
            .map(|c| (c.name, c.project))
            .collect();
        assert_eq!(
            rubies,
            [
                ("rbenv · Ruby 3.3.0".to_string(), true),
                ("chruby · Ruby 3.3.0".to_string(), true),
                ("rbenv · Ruby 3.2.2".to_string(), false)
            ]
        );
        let hls: Vec<String> = toolchain_candidates("haskell", &root, &host)
            .into_iter()
            .filter(|c| c.command.contains(".ghcup"))
            .map(|c| c.name)
            .collect();
        assert_eq!(hls, ["~/.ghcup/bin"]);
    }
}
