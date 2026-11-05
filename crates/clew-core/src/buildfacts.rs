//! What a project's build leaves out beyond the target platform
//! ([`clew_protocol::BuildFacts`]), for the reading aid that dims it
//! ([`crate::inactive`]): which features of each Cargo package are on, which
//! Go build tags are set, which macros C and C++ files are compiled with.
//!
//! Decided by the user's language choices ([`LanguageChoices`]), the
//! project's `.clew/lsp.toml` where they chose nothing, and its manifests —
//! read here, by reading files only, nothing run. Everything is kept to what
//! is known for certain: a feature another package of the workspace may turn
//! on is "maybe", a macro a compilation database may define is unknown, and
//! the reading aid never dims what it cannot decide.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use clew_protocol::{BuildFacts, CMacro, CrateFeatures, LanguageChoices, RustChoices};
use serde_json::Value;

use crate::lsp::config::ProjectLspConfig;
use crate::lsp::langopts::{self, read_toml, setting};

/// The crates of a project whose files are `rels`: the directories holding a
/// `Cargo.toml`, relative (`""` for the root), sorted.
pub fn crate_dirs<'a>(rels: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for rel in rels {
        let rel = rel.replace('\\', "/");
        if rel == "Cargo.toml" {
            dirs.insert(String::new());
        } else if let Some(dir) = rel.strip_suffix("/Cargo.toml") {
            dirs.insert(dir.to_string());
        }
    }
    dirs.into_iter().collect()
}

/// What the build of the project at `root` leaves out with the user's
/// `choices`, the project's `lsp.toml` (`config`) where they chose nothing,
/// and its manifests. `crates` are its crates' directories ([`crate_dirs`]).
/// Blocking: it reads the manifests.
pub fn read(
    root: &Path,
    crates: &[String],
    choices: &LanguageChoices,
    config: Option<&ProjectLspConfig>,
) -> BuildFacts {
    let options = |language: &str| {
        config
            .and_then(|c| c.resolve(language))
            .and_then(|s| s.init_options)
    };
    let rust = effective_rust(choices, options("rust").as_ref());
    let (goos, goarch, go_tags) = effective_go(choices, options("go").as_ref());
    let (c_defines, c_undefines) = effective_c(
        root,
        choices,
        options("c").or_else(|| options("cpp")).as_ref(),
    );
    BuildFacts {
        rust_target: rust
            .target
            .clone()
            .filter(|t| crate::lsp::langenv::is_target_triple(t)),
        crates: cargo_features(root, crates, &rust),
        goos,
        goarch,
        go_tags,
        c_defines,
        c_undefines,
    }
}

/// The Rust choices the build runs with: the user's, else what the
/// project's `lsp.toml` gives rust-analyzer.
fn effective_rust(choices: &LanguageChoices, options: Option<&Value>) -> RustChoices {
    if let Some(rust) = &choices.rust {
        return rust.clone();
    }
    let Some(options) = options else {
        return RustChoices::default();
    };
    let mut rust = RustChoices::default();
    match setting(options, "cargo.features") {
        Some(Value::String(all)) if all == "all" => rust.all_features = true,
        Some(Value::Array(list)) => {
            rust.features = list
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }
        _ => {}
    }
    rust.no_default_features = setting(options, "cargo.noDefaultFeatures")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    rust.target = setting(options, "cargo.target")
        .and_then(Value::as_str)
        .map(str::to_string);
    rust
}

/// One package of the workspace, as its manifest declares it.
struct Package {
    /// Its directory, relative to the project.
    dir: String,
    name: String,
    root: bool,
    /// Its `[features]` table.
    table: BTreeMap<String, Vec<String>>,
    /// Its optional dependencies that are features of their own name (none
    /// of its features names them as `dep:name`).
    implicit: BTreeSet<String>,
    /// Its dependencies on other packages of the workspace (`path`, or a
    /// workspace dependency with one).
    deps: Vec<Dep>,
}

/// A dependency of one workspace package on another.
struct Dep {
    /// The name it goes by in the dependent (its key).
    key: String,
    /// The package it is.
    package: String,
    features: Vec<String>,
    default_features: bool,
    optional: bool,
}

/// Each crate's features, for the packages the build loads: the root
/// package and the workspace's members.
fn cargo_features(root: &Path, crates: &[String], rust: &RustChoices) -> Vec<CrateFeatures> {
    let packages = workspace_packages(root);
    let chosen = crate::lsp::langenv::cargo_features(rust);
    let mut out: Vec<CrateFeatures> = Vec::new();
    for package in &packages {
        let mut declared: BTreeSet<String> = package.table.keys().cloned().collect();
        declared.extend(package.implicit.iter().cloned());
        let mut on: BTreeSet<String> = BTreeSet::new();
        let mut maybe: BTreeSet<String> = BTreeSet::new();
        if rust.all_features {
            on = declared.clone();
        } else {
            if !rust.no_default_features {
                on.insert("default".into());
            }
            for feature in &chosen {
                match feature.split_once('/') {
                    Some((name, feature)) if name == package.name => {
                        on.insert(feature.to_string());
                    }
                    Some(_) => {}
                    // A plain name is the root package's; in a workspace
                    // with none at its root, cargo may give it to members.
                    None if package.root => {
                        on.insert(feature.to_string());
                    }
                    None if !packages.iter().any(|p| p.root) => {
                        maybe.insert(feature.to_string());
                    }
                    None => {}
                }
            }
        }
        // What the workspace's other packages ask of this one: a dependency
        // they always build with is on, an optional one or a feature of
        // theirs (`dep/feature`) may be.
        for other in packages.iter().filter(|p| p.name != package.name) {
            for dep in other.deps.iter().filter(|d| d.package == package.name) {
                let wanted = match dep.optional {
                    true => &mut maybe,
                    false => &mut on,
                };
                wanted.extend(dep.features.iter().cloned());
                if dep.default_features {
                    wanted.insert("default".into());
                }
                for entries in other.table.values() {
                    for entry in entries {
                        let entry = entry.replacen('?', "", 1);
                        if let Some((key, feature)) = entry.split_once('/')
                            && key == dep.key
                        {
                            maybe.insert(feature.to_string());
                        }
                    }
                }
            }
        }
        // Only what the package declares: a `default` it has none of, a
        // name chosen for it that it does not have, is no feature of its.
        let on: BTreeSet<String> = closure(package, on)
            .intersection(&declared)
            .cloned()
            .collect();
        let maybe: BTreeSet<String> = closure(package, maybe)
            .intersection(&declared)
            .filter(|f| !on.contains(*f))
            .cloned()
            .collect();
        out.push(CrateFeatures {
            dir: package.dir.clone(),
            known: true,
            declared: declared.into_iter().collect(),
            on: on.into_iter().collect(),
            maybe: maybe.into_iter().collect(),
        });
    }
    // The project's other crates: not loaded, nothing known.
    for dir in crates {
        if !out.iter().any(|c| c.dir == *dir) {
            out.push(CrateFeatures {
                dir: dir.clone(),
                ..Default::default()
            });
        }
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

/// `start` and every feature of `package` they turn on, in turn.
fn closure(package: &Package, start: BTreeSet<String>) -> BTreeSet<String> {
    let mut on = BTreeSet::new();
    let mut work: Vec<String> = start.into_iter().collect();
    while let Some(feature) = work.pop() {
        if !on.insert(feature.clone()) {
            continue;
        }
        for entry in package.table.get(&feature).into_iter().flatten() {
            if entry.starts_with("dep:") {
                continue;
            }
            match entry.split_once('/') {
                // `dep/feature` also turns the optional dependency on — as
                // its feature, when it is one; `dep?/feature` does not.
                Some((dep, _)) if !dep.ends_with('?') && package.implicit.contains(dep) => {
                    work.push(dep.to_string());
                }
                Some(_) => {}
                None => work.push(entry.clone()),
            }
        }
    }
    on
}

/// The packages the build loads: the one at the root, and the workspace's
/// members.
fn workspace_packages(root: &Path) -> Vec<Package> {
    let Some(manifest) = read_toml(&root.join("Cargo.toml")) else {
        return Vec::new();
    };
    let shared = manifest
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    let mut packages = Vec::new();
    if let Some(package) = package(&manifest, String::new(), true, &shared) {
        packages.push(package);
    }
    for dir in langopts::workspace_members(root, &manifest) {
        let rel = dir
            .strip_prefix(root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if let Some(package) =
            read_toml(&dir.join("Cargo.toml")).and_then(|m| package(&m, rel, false, &shared))
        {
            packages.push(package);
        }
    }
    packages
}

/// The package `manifest` declares, at `dir`.
fn package(
    manifest: &toml::Table,
    dir: String,
    root: bool,
    shared: &toml::Table,
) -> Option<Package> {
    let name = manifest.get("package")?.get("name")?.as_str()?.to_string();
    let strings = |value: Option<&toml::Value>| -> Vec<String> {
        value
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let table: BTreeMap<String, Vec<String>> = manifest
        .get("features")
        .and_then(toml::Value::as_table)
        .map(|t| {
            t.iter()
                .map(|(k, v)| (k.clone(), strings(Some(v))))
                .collect()
        })
        .unwrap_or_default();
    let named_as_dep: BTreeSet<&str> = table
        .values()
        .flatten()
        .filter_map(|e| e.strip_prefix("dep:"))
        .collect();
    let mut tables: Vec<&toml::Table> = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        tables.extend(manifest.get(key).and_then(toml::Value::as_table));
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for target in targets.values().filter_map(toml::Value::as_table) {
            for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                tables.extend(target.get(key).and_then(toml::Value::as_table));
            }
        }
    }
    let mut implicit = BTreeSet::new();
    let mut deps = Vec::new();
    for deps_table in tables {
        for (key, spec) in deps_table {
            let spec = spec.as_table();
            let get = |k: &str| spec.and_then(|s| s.get(k));
            let optional = get("optional").and_then(toml::Value::as_bool) == Some(true);
            if optional && !named_as_dep.contains(key.as_str()) {
                implicit.insert(key.clone());
            }
            // A workspace dependency: its path, features and defaults are
            // the workspace's, its features added to.
            let inherited = (get("workspace").and_then(toml::Value::as_bool) == Some(true))
                .then(|| shared.get(key))
                .flatten()
                .and_then(toml::Value::as_table);
            let from = |k: &str| get(k).or_else(|| inherited.and_then(|i| i.get(k)));
            if from("path").is_none() {
                continue;
            }
            let mut features = strings(get("features"));
            features.extend(strings(inherited.and_then(|i| i.get("features"))));
            deps.push(Dep {
                key: key.clone(),
                package: from("package")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(key)
                    .to_string(),
                features,
                default_features: from("default-features")
                    .or_else(|| from("default_features"))
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(true),
                optional,
            });
        }
    }
    Some(Package {
        dir,
        name,
        root,
        table,
        implicit,
        deps,
    })
}

/// The Go platform and tags gopls loads the project with: the user's
/// choices, else the project's `lsp.toml`. The tags are `None` when a
/// `GOFLAGS` — the project's or this host's — sets some.
fn effective_go(
    choices: &LanguageChoices,
    options: Option<&Value>,
) -> (Option<String>, Option<String>, Option<Vec<String>>) {
    let env = |name: &str| {
        options
            .and_then(|o| setting(o, &format!("env.{name}")))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let goflags_tags = env("GOFLAGS")
        .into_iter()
        .chain(std::env::var("GOFLAGS").ok())
        .any(|flags| flags.contains("tags"));
    let (goos, goarch, tags) = match &choices.go {
        Some(go) => (
            go.goos.clone(),
            go.goarch.clone(),
            crate::lsp::langenv::go_tags_flag(go)
                .map(|flag| tags_of(&flag))
                .unwrap_or_default(),
        ),
        None => {
            let flags: Vec<String> = options
                .and_then(|o| setting(o, "buildFlags"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let mut tags = Vec::new();
            let mut words = flags.iter();
            while let Some(flag) = words.next() {
                let flag = flag.trim_start_matches('-');
                if let Some(list) = flag.strip_prefix("tags=") {
                    tags.extend(tags_of(list));
                } else if flag == "tags"
                    && let Some(list) = words.next()
                {
                    tags.extend(tags_of(list));
                }
            }
            (env("GOOS"), env("GOARCH"), tags)
        }
    };
    (goos, goarch, (!goflags_tags).then_some(tags))
}

/// The tags in a `-tags=a,b` flag (or its list alone).
fn tags_of(flag: &str) -> Vec<String> {
    flag.trim_start_matches("-tags=")
        .split([',', ' '])
        .map(str::trim)
        .filter(|t| crate::lsp::langenv::is_go_tag(t))
        .map(str::to_string)
        .collect()
}

/// The macros C/C++ files are compiled with, when the flags are known: a
/// root `compile_flags.txt` (clangd's flags for every file), else the
/// fallback flags — the user's, or the project's `lsp.toml`'s — but only
/// when no compilation database decides a file's flags instead.
fn effective_c(
    root: &Path,
    choices: &LanguageChoices,
    options: Option<&Value>,
) -> (Option<Vec<CMacro>>, Vec<String>) {
    let flags: Vec<String> =
        match crate::statefile::read_capped(&root.join("compile_flags.txt"), 1024 * 1024) {
            Some(text) => text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
            None => {
                let database = choices
                    .c
                    .as_ref()
                    .is_some_and(|c| c.compile_commands.is_some())
                    || options
                        .and_then(|o| setting(o, "compilationDatabasePath"))
                        .is_some()
                    || !langopts::compile_databases(root).is_empty();
                if database {
                    return (None, Vec::new());
                }
                match &choices.c {
                    Some(c) => c.flags.clone(),
                    None => options
                        .and_then(|o| setting(o, "fallbackFlags"))
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            }
        };
    let (defines, undefines) = macros_of(&flags);
    (Some(defines), undefines)
}

/// The macros `-D` defines and `-U` undefines in compile `flags`, the last
/// word for a name winning.
pub fn macros_of(flags: &[String]) -> (Vec<CMacro>, Vec<String>) {
    let mut state: BTreeMap<String, Option<Option<i64>>> = BTreeMap::new();
    let mut words = flags.iter().map(|f| f.trim());
    while let Some(flag) = words.next() {
        let (define, rest) = match flag {
            "-D" => (true, words.next().unwrap_or_default()),
            "-U" => (false, words.next().unwrap_or_default()),
            f if f.starts_with("-D") => (true, &f[2..]),
            f if f.starts_with("-U") => (false, &f[2..]),
            _ => continue,
        };
        let (name, value) = match rest.split_once('=') {
            Some((name, value)) => (name, number(value)),
            None => (rest, Some(1)),
        };
        if !is_macro_name(name) {
            continue;
        }
        state.insert(name.to_string(), define.then_some(value));
    }
    let mut defines = Vec::new();
    let mut undefines = Vec::new();
    for (name, value) in state {
        match value {
            Some(value) => defines.push(CMacro { name, value }),
            None => undefines.push(name),
        }
    }
    (defines, undefines)
}

/// A C identifier.
fn is_macro_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A C integer literal's value (`1`, `0x1F`, `017`, `10UL`), or `None` for
/// anything else.
pub fn number(text: &str) -> Option<i64> {
    let text = text
        .trim()
        .trim_end_matches(['u', 'U', 'l', 'L'])
        .replace('\'', "");
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).ok();
    }
    if let Some(bin) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
        return i64::from_str_radix(bin, 2).ok();
    }
    if text.len() > 1 && text.starts_with('0') {
        return i64::from_str_radix(&text[1..], 8).ok();
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use clew_protocol::{CChoices, GoChoices};

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn crate_of<'a>(facts: &'a BuildFacts, dir: &str) -> &'a CrateFeatures {
        facts.crates.iter().find(|c| c.dir == dir).unwrap()
    }

    /// The features each package builds with: its defaults and what they
    /// turn on, what the user chose (by package), what another member's
    /// dependency on it always asks — and, as "maybe", what one of its
    /// optional dependencies or features might. A crate outside the
    /// workspace is known for nothing.
    #[test]
    fn each_packages_features_are_worked_out_from_the_workspace() {
        let dir = TempDir::new("buildfacts-cargo");
        let root = dir.path();
        write(
            &root.join("Cargo.toml"),
            r#"
[package]
name = "app"
[features]
default = ["tls"]
tls = ["rustls/aws", "fast"]
fast = []
cli = ["dep:clap"]
extra = []
[dependencies]
rustls = { version = "1", optional = true }
clap = { version = "4", optional = true }
core = { path = "crates/core", features = ["simd"] }
[workspace]
members = ["crates/*"]
[workspace.dependencies]
util = { path = "crates/util", default-features = false }
"#,
        );
        write(
            &root.join("crates/core/Cargo.toml"),
            r#"
[package]
name = "core"
[features]
default = []
simd = []
serde = []
log = []
[dependencies]
util = { workspace = true, features = ["fmt"], optional = true }
"#,
        );
        write(
            &root.join("crates/util/Cargo.toml"),
            "[package]\nname = \"util\"\n[features]\ndefault = [\"std\"]\nstd = []\nfmt = []\n",
        );
        write(
            &root.join("fuzz/Cargo.toml"),
            "[package]\nname = \"fuzz\"\n",
        );
        let crates = crate_dirs([
            "Cargo.toml",
            "crates/core/Cargo.toml",
            "crates/util/Cargo.toml",
            "fuzz/Cargo.toml",
        ]);
        let choices = LanguageChoices {
            rust: Some(RustChoices {
                features: vec!["extra".into(), "core/serde".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let facts = read(root, &crates, &choices, None);
        let app = crate_of(&facts, "");
        assert!(app.known);
        // `tls` (a default) turns on `fast` and the optional `rustls`.
        assert_eq!(app.on, ["default", "extra", "fast", "rustls", "tls"]);
        assert!(app.declared.contains(&"cli".to_string()));
        assert!(
            !app.declared.contains(&"clap".to_string()),
            "named as dep:clap"
        );
        let core = crate_of(&facts, "crates/core");
        // `simd` because `app` always depends on it so; `serde` chosen.
        assert_eq!(core.on, ["default", "serde", "simd"]);
        assert!(core.maybe.is_empty());
        let util = crate_of(&facts, "crates/util");
        // Only `core`'s OPTIONAL dependency asks for `fmt` (and, with the
        // workspace's defaults off, nothing else).
        assert_eq!(util.on, ["default", "std"]);
        assert_eq!(util.maybe, ["fmt"]);
        assert!(!crate_of(&facts, "fuzz").known);

        // No defaults, and nothing chosen.
        let bare = read(
            root,
            &crates,
            &LanguageChoices {
                rust: Some(RustChoices {
                    no_default_features: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        );
        assert!(crate_of(&bare, "").on.is_empty());
        // All of them.
        let all = read(
            root,
            &crates,
            &LanguageChoices {
                rust: Some(RustChoices {
                    all_features: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        );
        assert_eq!(crate_of(&all, "").on, crate_of(&all, "").declared);
    }

    /// Where the user chose nothing, the project's lsp.toml decides — its
    /// Rust features, its Go tags, its C fallback flags.
    #[test]
    fn the_projects_lsp_toml_decides_where_nothing_was_chosen() {
        let dir = TempDir::new("buildfacts-lsptoml");
        let root = dir.path();
        write(
            &root.join("Cargo.toml"),
            "[package]\nname = \"app\"\n[features]\nfast = []\n",
        );
        write(
            &root.join(".clew/lsp.toml"),
            r#"
[rust.init_options]
"rust-analyzer.cargo.features" = ["fast"]
"rust-analyzer.cargo.target" = "wasm32-unknown-unknown"

[go.init_options]
buildFlags = ["-mod=vendor", "-tags=integration,e2e"]
env = { GOOS = "windows" }

[c.init_options]
fallbackFlags = ["-DDEBUG=2", "-UNDEBUG"]
"#,
        );
        let config = ProjectLspConfig::load(root).unwrap();
        let facts = read(
            root,
            &crate_dirs(["Cargo.toml"]),
            &Default::default(),
            Some(&config),
        );
        assert_eq!(crate_of(&facts, "").on, ["fast"]);
        assert_eq!(facts.rust_target.as_deref(), Some("wasm32-unknown-unknown"));
        assert_eq!(facts.goos.as_deref(), Some("windows"));
        assert_eq!(
            facts.go_tags,
            Some(vec!["integration".to_string(), "e2e".to_string()])
        );
        assert_eq!(
            facts.c_defines,
            Some(vec![CMacro {
                name: "DEBUG".into(),
                value: Some(2)
            }])
        );
        assert_eq!(facts.c_undefines, ["NDEBUG"]);

        // The user's choices win.
        let chosen = LanguageChoices {
            go: Some(GoChoices {
                tags: vec!["unit".into()],
                ..Default::default()
            }),
            c: Some(CChoices {
                flags: vec!["-D".into(), "MINE".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let facts = read(root, &[], &chosen, Some(&config));
        assert_eq!(facts.go_tags, Some(vec!["unit".to_string()]));
        assert_eq!(facts.goos, None);
        assert_eq!(
            facts.c_defines,
            Some(vec![CMacro {
                name: "MINE".into(),
                value: Some(1)
            }])
        );
    }

    /// A compilation database decides the flags of the files it covers:
    /// the fallback flags' macros are then not known — unless a root
    /// `compile_flags.txt` gives every file's.
    #[test]
    fn a_compilation_database_leaves_the_macros_unknown() {
        let dir = TempDir::new("buildfacts-ccdb");
        let root = dir.path();
        let chosen = LanguageChoices {
            c: Some(CChoices {
                flags: vec!["-DFAST".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(read(root, &[], &chosen, None).c_defines.is_some());
        write(&root.join("build/compile_commands.json"), "[]");
        assert_eq!(read(root, &[], &chosen, None).c_defines, None);
        write(
            &root.join("compile_flags.txt"),
            "-std=c11\n-DPLAIN\n-UGONE\n",
        );
        let facts = read(root, &[], &chosen, None);
        assert_eq!(
            facts.c_defines,
            Some(vec![CMacro {
                name: "PLAIN".into(),
                value: Some(1)
            }])
        );
        assert_eq!(facts.c_undefines, ["GONE"]);
    }

    #[test]
    fn integer_literals_read_as_c_does() {
        assert_eq!(number("10"), Some(10));
        assert_eq!(number("0x1F"), Some(31));
        assert_eq!(number("017"), Some(15));
        assert_eq!(number("10UL"), Some(10));
        assert_eq!(number("0"), Some(0));
        assert_eq!(number("\"text\""), None);
    }
}
