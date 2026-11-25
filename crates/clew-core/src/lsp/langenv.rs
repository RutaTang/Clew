//! Auto-detected per-project language environment, merged into the LSP
//! `initializationOptions` at launch — and the source of the interpreter a
//! Python program is debugged under ([`crate::debugadapter`]), so the debugger
//! and the language server see the same environment.
//!
//! Language servers need to know the project's environment to be accurate. The
//! needs cluster three ways (see the design notes):
//!   A. Toolchain the server can't find itself — Python venv, Java JDK, Zig
//!      compiler. clew locates it and passes it through. This module does (A).
//!   B. Build config that selects which code is active — Rust cargo
//!      features/target, Go build tags and platform, C/C++ flags. These are
//!      user *choices*, not auto-detectable: they come from `.clew/lsp.toml`
//!      init_options, or from Settings ([`chosen`]), which lists what the
//!      project offers to choose from ([`crate::lsp::langopts`]).
//!   C. Compilation database / project file the server locates itself — clangd's
//!      `compile_commands.json`, tsserver's `tsconfig.json`. Nothing to do.
//!
//! Explicit `init_options` from `lsp.toml` always win over what we auto-detect
//! — which is exactly why they carry the user's approval before they get here
//! (see [`merge`]): a repository value that wins over a vetted one is not a
//! preference, it is an override of this module's trust boundary.
//!
//! Over both lie the user's own choices for the project, made in Settings
//! ([`crate::lsp::choices`]): the interpreter they picked from the ones
//! [`python_candidates`] finds — found by looking, never by running one — or
//! named by path, and the build settings of (B). A choice picks the
//! environment; it does not lift the boundary ([`chosen_python_env`]).
//!
//! (A) reaches INTO the project for a path the server will then EXECUTE, so it
//! sits on the same trust boundary as an `lsp.toml` `command` — see
//! [`foreign_bytes`] and [`find_python_env`] for where that boundary is drawn
//! and what it does and does not cover. Every future (A) case (a JDK, a Zig compiler) picks an
//! executable out of the project the same way and must go through that same
//! gate; the Python venv is only the first one.
//!
//! Some servers (pyright) read these settings by *pulling*
//! `workspace/configuration` rather than from `initializationOptions`. The
//! LSP client answers those pulls from the very value it sent at `initialize`
//! ([`crate::lsp::client`], seeded into its server state at start), so
//! either way the server sees what this module computed.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clew_protocol::{
    CChoices, GoChoices, LanguageChoices, PythonCandidate, PythonEnvKind, PythonEnvironments,
    PythonInEffect, PythonSource, RustChoices,
};
use serde_json::{Value, json};

/// The auto-detected environment settings for `language`'s server in `root`, or
/// `None` when there is nothing to add.
pub fn detect(language: &str, _server: &str, root: &Path) -> Option<Value> {
    match language {
        "python" => Some(python_settings(&find_python_env(root)?)),
        "rust" => rust_projects(root),
        // Deno's server reads a project only where it is told to.
        "typescript" if _server == "deno" => Some(json!({ "enable": true, "lint": true })),
        "json" if _server == "vscode-json-language-server" => Some(json_settings()),
        _ => None,
    }
}

/// The schemas a JavaScript project's configuration files are checked and
/// completed against, by name — as VS Code associates them: `package.json`,
/// `tsconfig.json`, ESLint's, Prettier's, Babel's… A file that names its own
/// (`"$schema"`) is read by that one. The JSON server fetches each the first
/// time a file of it is opened.
pub const JSON_SCHEMAS: &[(&[&str], &str)] = &[
    (
        &["package.json"],
        "https://json.schemastore.org/package.json",
    ),
    (
        &["tsconfig.json", "tsconfig.*.json", "tsconfig-*.json"],
        "https://json.schemastore.org/tsconfig.json",
    ),
    (
        &["jsconfig.json", "jsconfig.*.json"],
        "https://json.schemastore.org/jsconfig.json",
    ),
    (
        &["deno.json", "deno.jsonc"],
        "https://raw.githubusercontent.com/denoland/deno/main/cli/schemas/config-file.v1.json",
    ),
    (
        &[".eslintrc", ".eslintrc.json"],
        "https://json.schemastore.org/eslintrc.json",
    ),
    (
        &[".prettierrc", ".prettierrc.json"],
        "https://json.schemastore.org/prettierrc.json",
    ),
    (
        &[".babelrc", ".babelrc.json", "babel.config.json"],
        "https://json.schemastore.org/babelrc.json",
    ),
    (&[".swcrc"], "https://json.schemastore.org/swcrc.json"),
    (
        &[".stylelintrc", ".stylelintrc.json"],
        "https://json.schemastore.org/stylelintrc.json",
    ),
    (&["turbo.json"], "https://turbo.build/schema.json"),
    (&["vercel.json"], "https://openapi.vercel.sh/vercel.json"),
    (&["lerna.json"], "https://json.schemastore.org/lerna.json"),
];

/// The JSON server's settings: [`JSON_SCHEMAS`], validation and formatting
/// on. It reads them only as they are pushed (`client::pushed_settings`).
fn json_settings() -> Value {
    let schemas: Vec<Value> = JSON_SCHEMAS
        .iter()
        .map(|(names, url)| json!({ "fileMatch": names, "url": url }))
        .collect();
    json!({
        "json": {
            "schemas": schemas,
            "validate": { "enable": true },
            "format": { "enable": true },
        }
    })
}

/// How deep under the project [`cargo_projects`] looks for a workspace:
/// `examples/Cargo.toml`, `tools/xtask/Cargo.toml` and the like.
const CARGO_DEPTH: usize = 4;
/// The most entries [`cargo_projects`] walks: a bound on a vast tree.
const CARGO_WALK_CAP: usize = 50_000;
/// A `Cargo.toml` larger than this is no project's manifest.
const MANIFEST_MAX_BYTES: u64 = 1 << 20;
/// The most projects [`cargo_projects`] links. A tree of more packages than
/// this, apart from any workspace, is not one rust-analyzer can load each
/// of in any case; past it, the walk stops reading manifests.
const CARGO_MAX_PROJECTS: usize = 64;

/// What a `Cargo.toml` declares, by its tables: a workspace, a package, or
/// neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestKind {
    Workspace,
    Package,
}

/// `manifest`'s kind, read from its table headers (`[workspace]`,
/// `[workspace.dependencies]`, `[package]`) and top-level dotted keys
/// (`workspace.members = …`) — not parsed whole: this runs as a language
/// server starts, over every manifest of a tree, and parsing each as TOML
/// held a window still for seconds on a tree of a thousand crates.
fn manifest_kind(manifest: &str) -> Option<ManifestKind> {
    let mut kind = None;
    let mut top_level = true;
    for line in manifest.lines() {
        let line = line.trim_start();
        let (key, is_header) = match line.strip_prefix('[') {
            // `[[bin]]` and the like: an array of tables, its own.
            Some(rest) if !rest.starts_with('[') => (rest.trim_start(), true),
            Some(_) => {
                top_level = false;
                continue;
            }
            None if top_level => (line, false),
            None => continue,
        };
        if is_header {
            top_level = false;
        }
        let named = |name: &str| {
            key.strip_prefix(name).is_some_and(|after| {
                let after = after.trim_start();
                if is_header {
                    after.starts_with(']') || after.starts_with('.')
                } else {
                    after.starts_with('.') || after.starts_with('=')
                }
            })
        };
        if named("workspace") {
            return Some(ManifestKind::Workspace);
        }
        if named("package") {
            kind = Some(ManifestKind::Package);
        }
    }
    kind
}

/// rust-analyzer's `linkedProjects` for `root`, when it holds Cargo
/// projects past the one at its top, which is all rust-analyzer finds of
/// itself: a repository whose `examples/` is a workspace of its own (as
/// many are, to keep the examples' dependencies out of the library's) had
/// every file in it read as part of no crate — no definition, no
/// implementation, no type, with the server saying it was ready.
fn rust_projects(root: &Path) -> Option<Value> {
    let projects = cargo_projects(root);
    let top_only = projects.len() == 1 && projects[0] == root.join("Cargo.toml");
    if projects.is_empty() || top_only {
        return None;
    }
    let linked: Vec<String> = projects
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    Some(json!({ "linkedProjects": linked }))
}

/// The Cargo projects under `root` a reader of it wants loaded: each
/// workspace (a `Cargo.toml` with `[workspace]`), and each package no
/// workspace above it holds. Within [`CARGO_DEPTH`], past what `.gitignore`
/// and the build directories leave out. Read, never run: a manifest is
/// parsed as TOML here, and only rust-analyzer — which the user lets build
/// the project — runs `cargo` on it.
pub fn cargo_projects(root: &Path) -> Vec<PathBuf> {
    let walker = ignore::WalkBuilder::new(root)
        .max_depth(Some(CARGO_DEPTH))
        .follow_links(false)
        .filter_entry(|entry| {
            !(entry.file_type().is_some_and(|t| t.is_dir())
                && crate::fs_scan::is_noise_dir(entry.file_name()))
        })
        .build();
    let mut workspaces: Vec<PathBuf> = Vec::new();
    let mut packages: Vec<PathBuf> = Vec::new();
    for entry in walker.flatten().take(CARGO_WALK_CAP) {
        if entry.file_name() != "Cargo.toml" || !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.into_path();
        // A crate `cargo vendor` copied into the project — it leaves the
        // crate's checksums beside its manifest — is one of its
        // dependencies, not a project: beside a package at the top, every
        // vendored crate was linked, and rust-analyzer loaded and built each
        // as a workspace of its own.
        if path.with_file_name(".cargo-checksum.json").is_file() {
            continue;
        }
        let too_big = std::fs::metadata(&path).map_or(true, |m| m.len() > MANIFEST_MAX_BYTES);
        let kind = (!too_big)
            .then(|| std::fs::read_to_string(&path).ok())
            .flatten()
            .and_then(|text| manifest_kind(&text));
        match kind {
            Some(ManifestKind::Workspace) => workspaces.push(path),
            Some(ManifestKind::Package) => packages.push(path),
            None => {}
        }
        if workspaces.len() + packages.len() >= CARGO_MAX_PROJECTS * 4 {
            break;
        }
    }
    // A package under a workspace's folder is that workspace's (or one it
    // leaves out, which is rare enough to read as its).
    let dirs: Vec<&Path> = workspaces.iter().filter_map(|w| w.parent()).collect();
    packages.retain(|p| {
        let Some(dir) = p.parent() else {
            return false;
        };
        !dirs.iter().any(|w| dir.starts_with(w))
    });
    let mut projects = workspaces;
    projects.extend(packages);
    projects.sort();
    projects.truncate(CARGO_MAX_PROJECTS);
    projects
}

/// The settings the user's `choices` give `language`'s server in `root`, or
/// `None` when they chose nothing for it, or what they chose cannot be used
/// (see [`chosen_python_env`]). Only what they set: a choice left unset
/// leaves the project's `lsp.toml`, or the server's default, to say.
pub fn chosen(language: &str, root: &Path, choices: &LanguageChoices) -> Option<Value> {
    match language {
        "python" => Some(python_settings(&chosen_python_env(
            root,
            choices.python.as_deref()?,
        )?)),
        "rust" => rust_settings(choices.rust.as_ref()?),
        "go" => go_settings(choices.go.as_ref()?),
        "c" | "cpp" => c_settings(root, choices.c.as_ref()?),
        "typescript" | "tsx" | "javascript" => {
            let tsserver = chosen_tsserver(choices.typescript.as_deref()?)?;
            Some(json!({ "tsserver": { "path": tsserver.to_string_lossy() } }))
        }
        "zig" => {
            let zig = crate::lsp::store::find_program(choices.zig.as_deref()?).ok()?;
            Some(json!({ "zig_exe_path": zig.to_string_lossy() }))
        }
        "java" => java_settings(choices.java.as_deref()?),
        "php" => {
            let version = php_version(choices.php.as_deref()?)?;
            Some(json!({ "intelephense": { "environment": { "phpVersion": version } } }))
        }
        "csharp" => {
            let solution = chosen_solution(root, choices.csharp.as_deref()?)?;
            Some(json!({ "csharp": { "solution": solution.to_string_lossy() } }))
        }
        _ => None,
    }
}

/// jdtls's settings for the JDK the user chose: the runtime the project is
/// built with (`java.configuration.runtimes`, named by its Java SE version
/// as jdtls names them), and `java.home`. `None` for a folder that holds no
/// JDK (no `bin/java`).
fn java_settings(chosen: &str) -> Option<Value> {
    let home = expand_home(chosen)?;
    let java = ["bin/java", "bin/java.exe"]
        .iter()
        .any(|b| home.join(b).is_file());
    if !java {
        return None;
    }
    let path = home.to_string_lossy().into_owned();
    let mut java = json!({ "home": path });
    if let Some(major) = crate::lsp::langopts::jdk_version(&home)
        .as_deref()
        .and_then(crate::lsp::langopts::java_major)
    {
        let name = match major {
            ..=8 => format!("JavaSE-1.{major}"),
            _ => format!("JavaSE-{major}"),
        };
        java["configuration"] = json!({
            "runtimes": [{ "name": name, "path": path, "default": true }],
        });
    }
    Some(json!({ "settings": { "java": java } }))
}

/// A PHP version as intelephense takes it (`8.2` → `8.2.0`), or `None` for
/// what is not one.
fn php_version(chosen: &str) -> Option<String> {
    let parts: Vec<&str> = chosen.trim().split('.').collect();
    let numbers = parts
        .iter()
        .all(|p| !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit()));
    match (numbers, parts.len()) {
        (true, 2) => Some(format!("{}.0", chosen.trim())),
        (true, 3) => Some(chosen.trim().to_string()),
        _ => None,
    }
}

/// The solution file the user chose for the C# server, made absolute
/// against the project; `None` for one that is not there.
fn chosen_solution(root: &Path, chosen: &str) -> Option<PathBuf> {
    let path = match expand_home(chosen) {
        Some(path) => path,
        None => root.join(chosen.trim()),
    };
    let solution = path.extension().is_some_and(|e| e == "sln" || e == "slnx");
    (solution && path.is_file()).then_some(path)
}

/// `chosen` as an absolute path — `~/` expanded — or `None` for a relative
/// one.
fn expand_home(chosen: &str) -> Option<PathBuf> {
    let chosen = chosen.trim();
    let path = match chosen.strip_prefix("~/") {
        Some(rest) => crate::home_dir()?.join(rest),
        None => PathBuf::from(chosen),
    };
    path.is_absolute().then_some(path)
}

/// The `tsserver.js` the user chose: the file, the TypeScript `lib` folder
/// holding it, or the package folder above that. `None` when there is none
/// there — and then clew's own TypeScript stays ([`pinned`]): a path the
/// server cannot use reads as unset to it, and it would fall back to the
/// project's copy, which runs only when chosen.
pub fn chosen_tsserver(chosen: &str) -> Option<PathBuf> {
    let chosen = chosen.trim();
    let path = match chosen.strip_prefix("~/") {
        Some(rest) => crate::home_dir()?.join(rest),
        None => PathBuf::from(chosen),
    };
    if !path.is_absolute() {
        return None;
    }
    [
        path.clone(),
        path.join("tsserver.js"),
        path.join("lib/tsserver.js"),
    ]
    .into_iter()
    .find(|p| p.file_name().is_some_and(|n| n == "tsserver.js") && p.is_file())
}

/// rust-analyzer's `cargo` settings for the user's Rust choices. Its check
/// (`check.features`, `check.targets`) follows these unless told otherwise.
fn rust_settings(rust: &RustChoices) -> Option<Value> {
    let mut cargo = serde_json::Map::new();
    if rust.all_features {
        cargo.insert("features".into(), json!("all"));
    } else {
        let features = cargo_features(rust);
        if !features.is_empty() {
            cargo.insert("features".into(), json!(features));
        }
    }
    if rust.no_default_features {
        cargo.insert("noDefaultFeatures".into(), json!(true));
    }
    if let Some(target) = rust.target.as_deref().map(str::trim)
        && is_target_triple(target)
    {
        cargo.insert("target".into(), json!(target));
    }
    (!cargo.is_empty()).then(|| json!({ "cargo": cargo }))
}

/// The chosen features cargo can take: `feature` or `package/feature`, each
/// part a plain name — what `cargo --features` accepts. Anything else is
/// dropped rather than handed on to fail the server's every build.
pub fn cargo_features(rust: &RustChoices) -> Vec<&str> {
    let plain = |part: &str| {
        !part.is_empty()
            && part.len() <= 128
            && part
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '+' | '.'))
    };
    rust.features
        .iter()
        .map(|f| f.trim())
        .filter(|f| match f.split_once('/') {
            Some((package, feature)) => plain(package) && plain(feature),
            None => plain(f),
        })
        .collect()
}

/// A target triple as rustc names one (`x86_64-unknown-linux-gnu`).
pub fn is_target_triple(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 128
        && target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// gopls's `buildFlags` and `env` for the user's Go choices.
fn go_settings(go: &GoChoices) -> Option<Value> {
    let mut out = serde_json::Map::new();
    if let Some(flag) = go_tags_flag(go) {
        out.insert("buildFlags".into(), json!([flag]));
    }
    let mut env = serde_json::Map::new();
    for (name, value) in [("GOOS", &go.goos), ("GOARCH", &go.goarch)] {
        if let Some(value) = value.as_deref().map(str::trim)
            && is_go_word(value)
        {
            env.insert(name.into(), json!(value));
        }
    }
    if !env.is_empty() {
        out.insert("env".into(), Value::Object(env));
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// The `-tags=…` flag for the chosen build tags — the go command's, so the
/// debugger builds with it too — or `None` for none. A tag is what a build
/// constraint can name (letters, digits, `_`, `.`): one with a comma or a
/// space in it would be several, or none.
pub fn go_tags_flag(go: &GoChoices) -> Option<String> {
    let tags: Vec<&str> = go
        .tags
        .iter()
        .map(|t| t.trim())
        .filter(|t| is_go_tag(t))
        .collect();
    (!tags.is_empty()).then(|| format!("-tags={}", tags.join(",")))
}

/// A name a `//go:build` line can use.
pub fn is_go_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 128
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
}

/// A `GOOS` or `GOARCH` value.
fn is_go_word(word: &str) -> bool {
    !word.is_empty()
        && word.len() <= 32
        && word
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// clangd's `compilationDatabasePath` and `fallbackFlags` for the user's
/// C/C++ choices. A database directory given relative to the project is
/// made absolute — clangd would read it against its own working directory.
fn c_settings(root: &Path, c: &CChoices) -> Option<Value> {
    let mut out = serde_json::Map::new();
    if let Some(dir) = c
        .compile_commands
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        let dir = match dir.strip_prefix("~/") {
            Some(rest) => match crate::home_dir() {
                Some(home) => home.join(rest),
                None => PathBuf::from(dir),
            },
            None => root.join(dir),
        };
        out.insert(
            "compilationDatabasePath".into(),
            json!(dir.to_string_lossy()),
        );
    }
    let flags: Vec<&str> = c
        .flags
        .iter()
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .collect();
    if !flags.is_empty() {
        out.insert("fallbackFlags".into(), json!(flags));
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// gopls takes its build flags as one list: the chosen `-tags` joins the
/// project's other flags (`-mod=vendor`, …) in place of any `-tags` of its,
/// rather than the list as a whole winning.
fn keep_build_flags(chosen: Option<Value>, explicit: Option<&Value>) -> Option<Value> {
    let mut chosen = chosen?;
    let theirs = explicit
        .and_then(|e| e.get("buildFlags"))
        .and_then(Value::as_array);
    if let (Some(theirs), Some(Value::Array(ours))) = (theirs, chosen.get_mut("buildFlags")) {
        let is_tags = |flag: &Value| {
            flag.as_str().is_some_and(|f| {
                let f = f.trim_start_matches('-');
                f == "tags" || f.starts_with("tags=")
            })
        };
        let mut flags: Vec<Value> = theirs.iter().filter(|f| !is_tags(f)).cloned().collect();
        flags.append(ours);
        *ours = flags;
    }
    Some(chosen)
}

/// What the Python servers read the environment from.
fn python_settings(env: &PythonEnv) -> Value {
    let path = |p: &Path| p.to_string_lossy().to_string();
    // Both keys, so pyright (defaultInterpreterPath) and pylsp
    // (pythonPath) each pick it up.
    let mut python = json!({
        "pythonPath": path(&env.run),
        "defaultInterpreterPath": path(&env.run),
    });
    let extra: Vec<String> = env.extra_paths.iter().map(|p| path(p)).collect();
    if !extra.is_empty() {
        python["analysis"] = json!({ "extraPaths": extra });
    }
    // The same for the other Python servers, each where it reads it:
    // basedpyright its own `analysis`, pylsp (pushed, it pulls nothing) its
    // Jedi plugin's, jedi-language-server its initialization options.
    let mut settings = json!({
        "python": python,
        "pylsp": { "plugins": { "jedi": {
            "environment": path(&env.run),
            "extra_paths": extra,
        } } },
        "workspace": {
            "environmentPath": path(&env.run),
            "extraPaths": extra,
        },
    });
    if !extra.is_empty() {
        settings["basedpyright"] = json!({ "analysis": { "extraPaths": extra } });
    }
    settings
}

/// Merge auto-detected settings under the explicit `lsp.toml` `init_options`,
/// with explicit values winning on conflict, and the user's `choices` over
/// both: what they chose in Settings is theirs to say, for their machine,
/// where the `lsp.toml` is the repository's. A language they chose for is
/// not detected at all — a project venv's search paths would otherwise
/// linger under the interpreter they picked instead.
///
/// `explicit` must already have passed the approval gate — `App::approved_
/// init_options` on the client, `clew_server::approved_init_options` on the
/// server. Nothing here can re-check it, and the merge is precisely where an
/// ungated value does its damage: a repository's `python.pythonPath` lands ON
/// TOP of the interpreter [`find_python_interpreter`] vetted, so it does not
/// merely bypass [`foreign_bytes`], it overrules it.
///
/// Beneath both lie the settings clew pins for its own store binary
/// ([`pinned`], with `exe` the binary about to run); `Err` when they cannot be
/// pinned, and then the server must not start.
pub fn merge(
    language: &str,
    server: &crate::lsp::config::EffectiveServer,
    exe: &Path,
    root: &Path,
    explicit: Option<Value>,
    choices: &LanguageChoices,
) -> Result<Option<Value>, String> {
    let chosen = chosen(language, root, choices);
    // What the user chose replaces what was found — an interpreter for an
    // interpreter — but for Rust they are of different things: the features
    // and target chosen, and the workspaces found to load.
    let detected = match chosen {
        Some(_) if language != "rust" => None,
        _ => detect(language, &server.server_name, root),
    };
    let chosen = match language {
        "go" => keep_build_flags(chosen, explicit.as_ref()),
        _ => chosen,
    };
    let auto = layer(layer(detected, explicit), chosen);
    Ok(layer(pinned(server, exe)?, auto))
}

/// `over` merged onto `base`, `over` winning on conflict.
fn layer(base: Option<Value>, over: Option<Value>) -> Option<Value> {
    match (base, over) {
        (Some(mut base), Some(over)) => {
            deep_merge(&mut base, over);
            Some(base)
        }
        (base, over) => base.or(over),
    }
}

/// Whether clew's own install of the server `server_name` is kept from
/// running the project's code by settings of clew's ([`pinned`]) — which a
/// program of the user's in its place runs without: one is not offered in
/// place of clew's install for it.
pub fn pins(server_name: &str) -> bool {
    matches!(
        server_name,
        "typescript-language-server" | "vue-typescript-language-server" | "astro-language-server"
    )
}

/// What TypeScript's server shows inline as a file is read: the type a
/// variable, a return or a property is inferred to have, the parameter a
/// literal argument fills (`connect(/* retries: */ 3)`), an enum member's
/// value — and not where the code already says it (a name that matches).
/// typescript-language-server answers no inlay hint without them.
fn typescript_preferences() -> Value {
    json!({
        "includeInlayParameterNameHints": "literals",
        "includeInlayParameterNameHintsWhenArgumentMatchesName": false,
        "includeInlayFunctionParameterTypeHints": false,
        "includeInlayVariableTypeHints": true,
        "includeInlayVariableTypeHintsWhenTypeMatchesName": false,
        "includeInlayPropertyDeclarationTypeHints": true,
        "includeInlayFunctionLikeReturnTypeHints": true,
        "includeInlayEnumMemberValueHints": true,
    })
}

/// The settings clew sends its own store binary `exe` for `server` in every
/// project, beneath everything else: the ones that keep the server from
/// running the project's code by default.
///
/// typescript-language-server prefers the TypeScript in the project's
/// `node_modules` to the one installed beside it, and runs it — so a
/// repository that ships `node_modules/typescript/lib/tsserver.js` had it
/// executed on opening a `.ts` file. `tsserver.path` names the copy clew
/// installed with the server instead. It must exist: the server reads a path
/// that does not as "unset" and goes back to the project's copy, so a missing
/// one is an `Err` and the server does not start. An approved `lsp.toml`
/// `tsserver.path` still wins, merged on top, as the user's own choice.
///
/// Nothing for a `command`: that is the repository's program, approved as
/// such. Nothing for a program the user chose either: theirs, set up as
/// they run it everywhere else — and not beside clew's install, where the
/// pin would look.
pub fn pinned(
    server: &crate::lsp::config::EffectiveServer,
    exe: &Path,
) -> Result<Option<Value>, String> {
    if server.command.is_some() || server.chosen.is_some() || !pins(&server.server_name) {
        return Ok(None);
    }
    // `<install>/node_modules/.bin/<server>`: what was installed with it is
    // in `<install>/node_modules`.
    let modules = exe.parent().and_then(Path::parent);
    let installed = |what: &str, parts: &[&str]| -> Result<std::path::PathBuf, String> {
        modules
            .map(|m| parts.iter().fold(m.to_path_buf(), |p, part| p.join(part)))
            .filter(|p| p.exists())
            .ok_or_else(|| {
                format!(
                    "clew's own {what} is missing from the {} install — without it the \
                     server would run the project's copy; remove the server and install it again",
                    server.server_name
                )
            })
    };
    match server.server_name.as_str() {
        "typescript-language-server" => {
            let tsserver = installed("TypeScript", &["typescript", "lib", "tsserver.js"])?;
            Ok(Some(json!({
                "tsserver": { "path": tsserver.to_string_lossy() },
                "preferences": typescript_preferences(),
            })))
        }
        // TypeScript's server for Vue components: clew's TypeScript, and
        // Vue's plugin from the same install (`location`, where tsserver
        // resolves it from), which it hands the components' documents.
        "vue-typescript-language-server" => {
            let tsserver = installed("TypeScript", &["typescript", "lib", "tsserver.js"])?;
            let plugin = installed("Vue plugin", &["@vue", "typescript-plugin"])?;
            let root = modules.and_then(Path::parent).unwrap_or(&plugin);
            Ok(Some(json!({
                "tsserver": { "path": tsserver.to_string_lossy() },
                "preferences": typescript_preferences(),
                "plugins": [{
                    "name": "@vue/typescript-plugin",
                    "location": root.to_string_lossy(),
                    "languages": ["vue"],
                }],
            })))
        }
        "astro-language-server" => {
            let tsdk = installed("TypeScript", &["typescript", "lib"])?;
            Ok(Some(json!({
                "typescript": { "tsdk": tsdk.to_string_lossy() }
            })))
        }
        _ => Ok(None),
    }
}

/// Byte cap for reading an interpreter to compare it against its base. A
/// CPython binary is single-digit megabytes; the cap is what stops a candidate
/// that the repository chose from being read without limit.
const MAX_INTERPRETER_BYTES: u64 = 64 * 1024 * 1024;

/// `pyvenv.cfg` is a handful of `key = value` lines, and it ships with the
/// repository, so its size is not trusted either.
const MAX_PYVENV_CFG_BYTES: u64 = 64 * 1024;

/// Locate a project-local Python interpreter (a virtualenv), or `None` to let
/// the server fall back to its own discovery.
///
/// The path returned is handed to the language server as `python.pythonPath` /
/// `defaultInterpreterPath`, and the Python server clew provisions (pyright)
/// RUNS it to enumerate `sys.path`. The search looks inside the project, so
/// every candidate here is a file that ships with the repository — the same
/// class of input as an `lsp.toml` `command`, which may only run after the
/// user has approved its fingerprint. Nothing on this path can ask for that
/// approval (it runs on every start, for a venv the user never chose *as a
/// clew setting*), so the boundary is drawn at the bytes instead:
/// [`foreign_bytes`] admits a candidate only when the bytes it runs are not
/// the repository's — and [`find_python_env`], what the language server is
/// handed, never runs a venv that came with the project, whose `.pth` files
/// would execute on start-up.
pub fn find_python_interpreter(root: &Path) -> Option<PathBuf> {
    find_vetted(root).map(|found| found.interpreter)
}

/// A virtualenv interpreter that passed [`foreign_bytes`].
struct Vetted {
    /// As found: `<venv>/bin/python`, never resolved.
    interpreter: PathBuf,
    /// The bytes executing it runs — a file outside the project.
    base: PathBuf,
    /// Found through `VIRTUAL_ENV`, the environment the user activated, not
    /// by a name inside the project.
    activated: bool,
    /// Kept for this project by its tool, outside it ([`find_managed`]): the
    /// user's own install, run as found, as an activated one is.
    managed: Option<PythonEnvKind>,
}

fn find_vetted(root: &Path) -> Option<Vetted> {
    find_vetted_on(root, &PythonHost::current())
}

fn find_vetted_on(root: &Path, host: &PythonHost) -> Option<Vetted> {
    const DIRS: [&str; 4] = PROJECT_VENV_DIRS;
    const EXES: [&str; 3] = ["bin/python", "bin/python3", "Scripts/python.exe"];
    // Containment is only decidable against the real root; without it there is
    // no way to tell a repository file from a system one, so detect nothing.
    let real_root = std::fs::canonicalize(root).ok()?;
    let repo = repository_top(&real_root);
    let vet = |cand: PathBuf, activated: bool| {
        if !cand.is_file() {
            return None;
        }
        foreign_bytes(&repo, &cand).map(|base| Vetted {
            interpreter: cand,
            base,
            activated,
            managed: None,
        })
    };
    // uv's environment for the project, where `UV_PROJECT_ENVIRONMENT`
    // moves it: one the user set outside the project is theirs, as an
    // activated one is.
    if let Some(env) = uv_project_env(root, host) {
        let theirs = !env.starts_with(root);
        for exe in EXES {
            if let Some(found) = vet(env.join(exe), theirs) {
                return Some(found);
            }
        }
    }
    // Project-local virtualenvs, most-conventional first.
    for dir in DIRS {
        for exe in EXES {
            if let Some(found) = vet(root.join(dir).join(exe), false) {
                return Some(found);
            }
        }
    }
    // The interpreter PDM records for the project: one of the project's
    // own is read as any venv in it is; one outside it is the user's.
    if let Some(cand) = pdm_python(root) {
        let outside = std::fs::canonicalize(&cand).is_ok_and(|real| !real.starts_with(&repo));
        if let Some(mut found) = vet(cand, outside) {
            if outside {
                found.managed = Some(PythonEnvKind::Pdm);
            }
            return Some(found);
        }
    }
    // An already-activated virtualenv in the environment, or conda
    // environment. Held to the SAME check: `VIRTUAL_ENV` routinely names the
    // project's own `.venv`, and one file must not get two different answers
    // depending on which name reached it.
    // Conda's `base`, active in every shell by default, is no environment
    // chosen for this project: it is looked at after the project's own.
    let activated = |var: &str| {
        let env = host.dir(var)?;
        EXES.into_iter()
            .chain(["python.exe"])
            .find_map(|exe| vet(env.join(exe), true))
    };
    if let Some(found) = activated("VIRTUAL_ENV").or_else(|| {
        (!host.conda_is_base())
            .then(|| activated("CONDA_PREFIX"))
            .flatten()
    }) {
        return Some(found);
    }
    // This project's environment as its tool keeps it, outside the project:
    // a Poetry project opened with nothing activated had its imports read
    // against whatever `python3` came first, and every import of its
    // packages was unresolved.
    let Some((interpreter, kind)) = find_managed(root, host) else {
        // A monorepo whose one Python project keeps its venv a folder down
        // (`backend/.venv`): that one. Two or more is a choice to make in
        // Settings, never a guess.
        let mut below = sorted_dirs(root)
            .into_iter()
            .filter(|child| !dir_name(child).starts_with('.'))
            .flat_map(|child| [child.join(".venv"), child.join("venv")])
            .filter_map(|venv| EXES.iter().find_map(|exe| vet(venv.join(exe), false)));
        if let (Some(only), None) = (below.next(), below.next()) {
            return Some(only);
        }
        return host
            .conda_is_base()
            .then(|| activated("CONDA_PREFIX"))
            .flatten();
    };
    Some(Vetted {
        base: interpreter.clone(),
        interpreter,
        activated: true,
        managed: Some(kind),
    })
}

/// This project's own environment as the tool that made it keeps it,
/// outside the project — found by working out where that tool put it, the
/// way it names it, never by running the tool: its Poetry environment
/// (`<name>-<hash of its folder>-py3.x`, the Python Poetry's `envs.toml`
/// records as in use first), its Pipenv one (`<folder>-<hash of its
/// Pipfile>`), and the pyenv Python its `.python-version` pins. Each is the
/// user's own install, outside the repository: what the repository says
/// only picks among them.
fn find_managed(root: &Path, host: &PythonHost) -> Option<(PathBuf, PythonEnvKind)> {
    let real_root = std::fs::canonicalize(root).ok()?;
    let real = poetry_cwd(&real_root);
    if let Some(dir) = poetry_dir(host) {
        let name = format!(
            "{}-{}",
            poetry_sanitized(&project_name(root)),
            path_hash(real.as_bytes(), 32)
        );
        let name_prefix = name.clone();
        // The Python Poetry uses for it (`poetry env use`), else the newest.
        let in_use = crate::statefile::read_capped(&dir.join("envs.toml"), MAX_PYPROJECT_BYTES)
            .and_then(|text| text.parse::<toml::Table>().ok())
            .and_then(|envs| {
                let minor = envs.get(&name)?.get("minor")?.as_str()?.to_string();
                Some(format!("{name}-py{minor}"))
            });
        // Its Pythons, by version (`-py3.9` before `-py3.12`), the one in
        // use last.
        let python_of = |env: &PathBuf| -> Option<Vec<u32>> {
            let name = dir_name(env);
            let version = name
                .strip_prefix(&name_prefix)?
                .strip_prefix("-py")?
                .to_string();
            version.split('.').map(|n| n.parse().ok()).collect()
        };
        let mut envs: Vec<(bool, Vec<u32>, PathBuf)> = sorted_dirs(&dir)
            .into_iter()
            .filter_map(|env| {
                let version = python_of(&env)?;
                Some((Some(dir_name(&env)) == in_use, version, env))
            })
            .collect();
        envs.sort();
        if let Some(found) = envs
            .iter()
            .rev()
            .find_map(|(_, _, env)| interpreter_in(env))
        {
            return Some((found, PythonEnvKind::Poetry));
        }
    }
    let pipfile = real_root.join("Pipfile");
    if pipfile.is_file()
        && let Some(dir) = pipenv_dir(host)
    {
        let folder = real_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = format!(
            "{}-{}",
            pipenv_sanitized(&folder),
            path_hash(pipfile.to_string_lossy().as_bytes(), 6)
        );
        if let Some(found) = interpreter_in(&dir.join(name)) {
            return Some((found, PythonEnvKind::Pipenv));
        }
    }
    if let Some(found) = hatch_envs(host, &project_name(root))
        .iter()
        .find_map(|env| interpreter_in(env))
    {
        return Some((found, PythonEnvKind::Hatch));
    }
    if let Some(found) = conda_env_named(root, host).and_then(|env| interpreter_in(&env)) {
        return Some((found, PythonEnvKind::Conda));
    }
    // The Python `.python-version` pins: pyenv's, else one uv installed
    // (`cpython-3.12.4-linux-x86_64-gnu`). `3.12` pins the newest 3.12.
    let pins = pinned_versions(root);
    if pins.is_empty() {
        return None;
    }
    let pyenv = pyenv_versions(host);
    if let Some(found) = pinned_dir(&pyenv, &pins, |name| Some(name.to_string()))
        .and_then(|dir| interpreter_in(&dir))
    {
        return Some((found, PythonEnvKind::Pyenv));
    }
    let uv = uv_pythons(host);
    pinned_dir(&uv, &pins, |name| {
        name.split('-').nth(1).map(str::to_string)
    })
    .and_then(|dir| interpreter_in(&dir))
    .map(|found| (found, PythonEnvKind::Uv))
}

/// The Pythons uv installed on `host`.
fn uv_pythons(host: &PythonHost) -> Vec<PathBuf> {
    host.dir("UV_PYTHON_INSTALL_DIR")
        .or_else(|| match () {
            _ if host.windows => host.dir("APPDATA").map(|d| d.join("uv/python")),
            _ => host
                .dir("XDG_DATA_HOME")
                .or_else(|| host.home().map(|h| h.join(".local/share")))
                .map(|d| d.join("uv/python")),
        })
        .map(|dir| sorted_dirs(&dir))
        .unwrap_or_default()
}

/// The first eight characters of the URL-safe base64 of `bytes`' SHA-256,
/// cut to its first `keep` bytes: how Poetry (all 32) and Pipenv (6) tell a
/// project's environment from another of the same name.
fn path_hash(bytes: &[u8], keep: usize) -> String {
    use base64::Engine;
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    let encoded = base64::engine::general_purpose::URL_SAFE.encode(&digest[..keep.min(32)]);
    encoded.chars().take(8).collect()
}

/// The Python environment the user chose (`chosen`, a path on this host, as
/// they gave it), made safe to run — or `None` when it cannot be used: it is
/// not an absolute path to a file, or it is the repository's own program.
///
/// A choice names the environment; it does not lift the boundary
/// [`find_python_env`] draws. Outside the repository (a Poetry, conda, pyenv
/// or uv environment, a system Python) it is the user's own install, and is
/// run as found. Inside it, it is held to the same check as a venv found
/// there by name, and run the same way: through the base interpreter its
/// bytes are, its packages read — a repository can ship a `.venv` as easily
/// as anything else, and choosing it from a list does not tell the user
/// whose its start-up code is.
fn chosen_python_env(root: &Path, chosen: &str) -> Option<PythonEnv> {
    let cand = Path::new(chosen);
    match chosen_in_repository(root, cand)? {
        None => Some(PythonEnv {
            run: cand.to_path_buf(),
            extra_paths: Vec::new(),
        }),
        Some(repo) => {
            let base = foreign_bytes(&repo, cand)?;
            let venv = cand.parent()?.parent()?;
            Some(reading_env(root, base, site_paths(venv)))
        }
    }
}

/// The interpreter the user chose (`chosen`), to debug a program in `root`
/// under — as found, a venv's interpreter booting its environment — or
/// `None` when it cannot be used. Inside the repository it must pass the
/// same check as a venv found there ([`foreign_bytes`]); what then runs is
/// the project's program, which is what debugging it asks for.
pub fn chosen_interpreter(root: &Path, chosen: &str) -> Option<PathBuf> {
    let cand = Path::new(chosen);
    match chosen_in_repository(root, cand)? {
        None => Some(cand.to_path_buf()),
        Some(repo) => foreign_bytes(&repo, cand).map(|_| cand.to_path_buf()),
    }
}

/// Whether the interpreter `cand` (or the environment around it) lies in the
/// repository holding `root`: `Some(Some(repository))` when it does,
/// `Some(None)` when it does not, `None` when it cannot be used at all — not
/// an absolute path to a file, or the project cannot be resolved to tell.
fn chosen_in_repository(root: &Path, cand: &Path) -> Option<Option<PathBuf>> {
    if !cand.is_absolute() || !cand.is_file() {
        return None;
    }
    let real_root = std::fs::canonicalize(root).ok()?;
    let repo = repository_top(&real_root);
    let inside = |p: &Path| std::fs::canonicalize(p).is_ok_and(|real| real.starts_with(&repo));
    // The environment, not only the file: `<venv>/bin/python` links out of
    // the project, but `<venv>` — its `pyvenv.cfg`, its `.pth` files — is
    // what running it boots.
    let env = cand.parent().and_then(Path::parent);
    Some((inside(cand) || env.is_some_and(inside)).then_some(repo))
}

/// What the Python language server in `root` is given, and who named it: the
/// user's `choices`, else the project's `lsp.toml` options (`lsp_toml`),
/// else the virtualenv detection finds — the order [`merge`] layers them in.
pub fn python_in_effect(
    root: &Path,
    choices: &LanguageChoices,
    lsp_toml: Option<&Value>,
) -> PythonInEffect {
    let mut unusable_choice = None;
    if let Some(chosen) = &choices.python {
        if chosen_python_env(root, chosen).is_some() {
            return PythonInEffect {
                interpreter: Some(chosen.clone()),
                source: PythonSource::Chosen,
                unusable_choice: None,
            };
        }
        unusable_choice = Some(chosen.clone());
    }
    let named = lsp_toml
        .and_then(|options| options.get("python"))
        .and_then(|python| {
            python
                .get("pythonPath")
                .or_else(|| python.get("defaultInterpreterPath"))
        })
        .and_then(Value::as_str);
    let (interpreter, source) = match named {
        Some(path) => (Some(path.to_string()), PythonSource::LspToml),
        None => match find_vetted(root) {
            Some(found) => (
                Some(found.interpreter.to_string_lossy().into_owned()),
                match (found.managed, found.activated) {
                    (Some(kind), _) => PythonSource::Managed(kind),
                    (None, true) => PythonSource::Activated,
                    (None, false) => PythonSource::ProjectVenv,
                },
            ),
            None => (None, PythonSource::Server),
        },
    };
    PythonInEffect {
        interpreter,
        source,
        unusable_choice,
    }
}

/// The project's Python interpreters on this host, and the one its language
/// server is given ([`python_candidates`], [`python_in_effect`]).
pub fn python_environments(
    root: &Path,
    choices: &LanguageChoices,
    lsp_toml: Option<&Value>,
) -> PythonEnvironments {
    PythonEnvironments {
        candidates: python_candidates(root, &PythonHost::current()),
        in_effect: python_in_effect(root, choices, lsp_toml),
    }
}

/// Where a host keeps its Python installations, as its environment says.
/// Passed in, so a test can point it at a scratch home.
pub struct PythonHost {
    /// The user's home.
    pub home: Option<PathBuf>,
    /// The search path `python3` is looked for on.
    pub path: Option<OsString>,
    /// The environment variables that move where the tools keep their
    /// environments (see [`PYTHON_HOST_VARS`]), as set.
    pub vars: Vec<(&'static str, OsString)>,
    /// Whether this is a Mac, where caches live under `~/Library/Caches`.
    pub macos: bool,
    /// Whether this is Windows, where tools keep their environments under
    /// `%LOCALAPPDATA%` and `%APPDATA%`, and pyenv is pyenv-win.
    pub windows: bool,
}

/// The environment variables [`PythonHost`] reads.
const PYTHON_HOST_VARS: &[&str] = &[
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
    "POETRY_VIRTUALENVS_PATH",
    "WORKON_HOME",
    "PYENV_ROOT",
    "UV_PYTHON_INSTALL_DIR",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "UV_PROJECT_ENVIRONMENT",
    "HATCH_DATA_DIR",
    "POETRY_CACHE_DIR",
    "POETRY_CONFIG_DIR",
    "LOCALAPPDATA",
    "APPDATA",
];

impl PythonHost {
    /// This process's host.
    pub fn current() -> PythonHost {
        PythonHost {
            home: crate::home_dir(),
            path: crate::linerpc::spawn_path(),
            vars: PYTHON_HOST_VARS
                .iter()
                .filter_map(|&name| Some((name, std::env::var_os(name)?)))
                .collect(),
            macos: cfg!(target_os = "macos"),
            windows: cfg!(windows),
        }
    }

    /// The raw value of variable `name`, as set.
    fn var(&self, name: &str) -> Option<&OsString> {
        self.vars.iter().find(|(n, _)| *n == name).map(|(_, v)| v)
    }

    /// The directory variable `name` names, when it names an absolute one.
    fn dir(&self, name: &str) -> Option<PathBuf> {
        self.vars
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| PathBuf::from(v))
            .filter(|p| p.is_absolute())
    }

    /// Whether the conda environment active is conda's own `base` — the
    /// one conda activates in every shell by default (`auto_activate_base`),
    /// not one the user activated for this project.
    fn conda_is_base(&self) -> bool {
        self.vars
            .iter()
            .any(|(n, v)| *n == "CONDA_DEFAULT_ENV" && v == "base")
    }

    /// The user's home, when absolute.
    fn home(&self) -> Option<&Path> {
        self.home.as_deref().filter(|h| h.is_absolute())
    }
}

/// Most interpreters listed for one project.
const MAX_CANDIDATES: usize = 64;
/// Most entries read from one directory of environments.
const MAX_ENV_ENTRIES: usize = 256;
/// `pyproject.toml`, read for the project's name; it ships with the
/// repository.
const MAX_PYPROJECT_BYTES: u64 = 256 * 1024;
/// Conda's list of environments, one path a line.
const MAX_CONDA_LIST_BYTES: u64 = 64 * 1024;

/// The Python interpreters a user may run the project's language server
/// under, found by LOOKING where each tool keeps them — none is run, as one
/// in the project could be the repository's program: the project's own
/// virtualenvs (in its folder or a folder down), the environment activated
/// where clew started, this project's Poetry and Pipenv environments, conda's
/// environments, pyenv's and uv's Pythons, and a `python3` on the search path
/// or where systems keep one. Most specific first, each interpreter once,
/// [`MAX_CANDIDATES`] at most.
///
/// A virtualenv in the project is listed only when the bytes running it would
/// run are not the repository's ([`foreign_bytes`]) — the check detection
/// applies — so the list never offers a planted `.venv/bin/python` as the
/// user's environment.
pub fn python_candidates(root: &Path, host: &PythonHost) -> Vec<PythonCandidate> {
    let mut out: Vec<PythonCandidate> = Vec::new();
    let add = |out: &mut Vec<PythonCandidate>,
               interpreter: PathBuf,
               kind: PythonEnvKind,
               name: String,
               version: Option<String>| {
        let interpreter = interpreter.to_string_lossy().into_owned();
        if out.len() < MAX_CANDIDATES && !out.iter().any(|c| c.interpreter == interpreter) {
            out.push(PythonCandidate {
                interpreter,
                kind,
                name,
                version,
            });
        }
    };
    // The project's own, vetted as detection vets them.
    if let Ok(real_root) = std::fs::canonicalize(root) {
        let repo = repository_top(&real_root);
        let mut dirs: Vec<(String, PathBuf)> = PROJECT_VENV_DIRS
            .iter()
            .map(|d| (d.to_string(), root.join(d)))
            .collect();
        for child in sorted_dirs(root) {
            let Some(name) = child
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            for d in [".venv", "venv"] {
                dirs.push((format!("{name}/{d}"), child.join(d)));
            }
        }
        for (name, dir) in dirs {
            if let Some(interpreter) = interpreter_in(&dir)
                && foreign_bytes(&repo, &interpreter).is_some()
            {
                add(
                    &mut out,
                    interpreter,
                    PythonEnvKind::Project,
                    name,
                    pyvenv_version(&dir),
                );
            }
        }
    }
    for var in ["VIRTUAL_ENV", "CONDA_PREFIX"] {
        if let Some(dir) = host.dir(var)
            && let Some(interpreter) = interpreter_in(&dir)
        {
            let version = pyvenv_version(&dir).or_else(|| conda_version(&dir));
            add(
                &mut out,
                interpreter,
                PythonEnvKind::Activated,
                dir_name(&dir),
                version,
            );
        }
    }
    let project = project_name(root);
    if let Some(dir) = poetry_dir(host) {
        let prefix = format!("{}-", poetry_sanitized(&project));
        for env in sorted_dirs(&dir) {
            let name = dir_name(&env);
            if name.starts_with(&prefix)
                && let Some(interpreter) = interpreter_in(&env)
            {
                add(
                    &mut out,
                    interpreter,
                    PythonEnvKind::Poetry,
                    name,
                    pyvenv_version(&env),
                );
            }
        }
    }
    for env in hatch_envs(host, &project) {
        if let Some(interpreter) = interpreter_in(&env) {
            add(
                &mut out,
                interpreter,
                PythonEnvKind::Hatch,
                dir_name(&env),
                pyvenv_version(&env),
            );
        }
    }
    if let Some(cand) = pdm_python(root).filter(|p| p.is_file())
        && std::fs::canonicalize(&cand).is_ok_and(|real| {
            std::fs::canonicalize(root).is_ok_and(|root| !real.starts_with(repository_top(&root)))
        })
    {
        let name = cand
            .parent()
            .and_then(|bin| bin.parent())
            .map(dir_name)
            .unwrap_or_default();
        add(&mut out, cand, PythonEnvKind::Pdm, name, None);
    }
    if let Some(dir) = pipenv_dir(host) {
        let folder = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let prefix = format!("{}-", pipenv_sanitized(&folder));
        for env in sorted_dirs(&dir) {
            let name = dir_name(&env);
            if name.starts_with(&prefix)
                && let Some(interpreter) = interpreter_in(&env)
            {
                add(
                    &mut out,
                    interpreter,
                    PythonEnvKind::Pipenv,
                    name,
                    pyvenv_version(&env),
                );
            }
        }
    }
    for (name, env) in conda_envs(host) {
        if let Some(interpreter) = interpreter_in(&env) {
            add(
                &mut out,
                interpreter,
                PythonEnvKind::Conda,
                name,
                conda_version(&env),
            );
        }
    }
    {
        // The versions the project pins (`.python-version`) first.
        let pins = pinned_versions(root);
        let mut versions = pyenv_versions(host);
        versions.sort_by_key(|v| !pins.iter().any(|pin| version_pinned(&dir_name(v), pin)));
        for env in versions {
            let name = dir_name(&env);
            if let Some(interpreter) = interpreter_in(&env) {
                let version = name
                    .starts_with(|c: char| c.is_ascii_digit())
                    .then(|| name.clone());
                add(&mut out, interpreter, PythonEnvKind::Pyenv, name, version);
            }
        }
    }
    {
        for env in uv_pythons(host) {
            let name = dir_name(&env);
            if let Some(interpreter) = interpreter_in(&env) {
                // `cpython-3.12.4-macos-aarch64-none`.
                let version = name.split('-').nth(1).map(str::to_string);
                add(&mut out, interpreter, PythonEnvKind::Uv, name, version);
            }
        }
    }
    let mut system: Vec<PathBuf> = Vec::new();
    if let Some(path) = &host.path {
        for dir in std::env::split_paths(path).filter(|d| d.is_absolute()) {
            system.push(dir.join("python3"));
        }
    }
    for well_known in [
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
        "/usr/bin/python3",
    ] {
        system.push(PathBuf::from(well_known));
    }
    // One program by two of the search path's names — /bin/python3 and
    // /usr/bin/python3, where /bin is a link to /usr/bin — is listed once,
    // by the first. Its folder is what is followed, not the file: a
    // virtualenv's python links to the system's and is still its own.
    let mut places = std::collections::HashSet::new();
    for interpreter in system.into_iter().filter(|p| p.is_file()) {
        let place = interpreter
            .parent()
            .and_then(|dir| std::fs::canonicalize(dir).ok())
            .zip(interpreter.file_name())
            .map(|(dir, file)| dir.join(file));
        if place.is_some_and(|place| !places.insert(place)) {
            continue;
        }
        let name = interpreter.to_string_lossy().into_owned();
        add(&mut out, interpreter, PythonEnvKind::System, name, None);
    }
    out
}

/// The folders a project's own virtualenv goes by, most conventional first.
const PROJECT_VENV_DIRS: [&str; 4] = [".venv", "venv", ".env", "env"];

/// Where a virtualenv's (or a conda environment's, or an installed Python's)
/// interpreter is, when it has one.
fn interpreter_in(env: &Path) -> Option<PathBuf> {
    [
        "bin/python",
        "bin/python3",
        "Scripts/python.exe",
        "python.exe",
    ]
    .into_iter()
    .map(|exe| env.join(exe))
    .find(|p| p.is_file())
}

/// The folders in `dir`, sorted, [`MAX_ENV_ENTRIES`] at most.
fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .take(MAX_ENV_ENTRIES)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// The last part of `dir`'s name.
fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The Python version a virtualenv's `pyvenv.cfg` records.
fn pyvenv_version(venv: &Path) -> Option<String> {
    let cfg = crate::statefile::read_capped(&venv.join("pyvenv.cfg"), MAX_PYVENV_CFG_BYTES)?;
    cfg.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        matches!(key.trim(), "version" | "version_info").then(|| value.trim().to_string())
    })
}

/// The Python version a conda environment holds, from the package record
/// conda keeps of it (`conda-meta/python-3.12.4-h….json`).
fn conda_version(env: &Path) -> Option<String> {
    let Ok(entries) = std::fs::read_dir(env.join("conda-meta")) else {
        return None;
    };
    entries.flatten().take(4096).find_map(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        let rest = name.strip_prefix("python-")?;
        rest.starts_with(|c: char| c.is_ascii_digit())
            .then(|| rest.split('-').next().unwrap_or(rest).to_string())
    })
}

/// The project's name, as Poetry names its environments after it: its
/// `pyproject.toml`'s (`[tool.poetry]` or `[project]`), else its folder's.
fn project_name(root: &Path) -> String {
    let declared = crate::statefile::read_capped(&root.join("pyproject.toml"), MAX_PYPROJECT_BYTES)
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| {
            let name = |t: &toml::Table| t.get("name")?.as_str().map(str::to_string);
            table
                .get("tool")
                .and_then(|t| t.get("poetry"))
                .and_then(|p| p.as_table())
                .and_then(name)
                .or_else(|| table.get("project")?.as_table().and_then(name))
        });
    declared.unwrap_or_else(|| {
        root.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    })
}

/// The project's folder as Poetry hashes it: `os.path.normcase(realpath)`
/// — on Windows lowercased, without the `\\?\` that canonicalizing adds,
/// which no hash Poetry made has.
fn poetry_cwd(real: &Path) -> String {
    let path = real.to_string_lossy();
    if !cfg!(windows) {
        return path.into_owned();
    }
    let plain = match path.strip_prefix(r"\\?\UNC\") {
        Some(share) => format!(r"\\{share}"),
        None => path.strip_prefix(r"\\?\").unwrap_or(&path).to_string(),
    };
    plain.replace('/', "\\").to_lowercase()
}

/// A project name as Poetry spells it in an environment's folder name:
/// its name as packages are named (PEP 503: lowercased, each run of `-`,
/// `_` and `.` one `-` — `my_pkg` is `my-pkg`), its shell-unfriendly
/// characters `_`, 42 characters at most.
fn poetry_sanitized(name: &str) -> String {
    let mut canonical = String::with_capacity(name.len());
    for c in name.to_lowercase().chars() {
        if matches!(c, '-' | '_' | '.') {
            if !canonical.ends_with('-') {
                canonical.push('-');
            }
        } else {
            canonical.push(c);
        }
    }
    canonical
        .chars()
        .map(|c| match c {
            ' ' | '$' | '`' | '!' | '*' | '@' | '"' | '\\' | '\r' | '\n' | '\t' => '_',
            c => c,
        })
        .take(42)
        .collect()
}

/// A project folder's name as Pipenv spells it in an environment's folder
/// name.
fn pipenv_sanitized(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            ' ' | '&' | '$' | '`' | '!' | '*' | '@' | '"' | '(' | ')' | '[' | ']' | '\\' | '\r'
            | '\n' | '\t' => '_',
            c => c,
        })
        .take(42)
        .collect()
}

/// Where Poetry keeps its environments on `host`: `POETRY_VIRTUALENVS_PATH`,
/// else `virtualenvs.path` of its `config.toml` (`{cache-dir}` in it
/// spelled out), else `virtualenvs` in its cache: the cache `cache-dir`
/// names, else the platform's (`%LOCALAPPDATA%\pypoetry\Cache` on Windows,
/// `~/Library/Caches/pypoetry` on a Mac, `$XDG_CACHE_HOME/pypoetry`).
fn poetry_dir(host: &PythonHost) -> Option<PathBuf> {
    if let Some(dir) = host.dir("POETRY_VIRTUALENVS_PATH") {
        return Some(dir);
    }
    let config_dir = host.dir("POETRY_CONFIG_DIR").or_else(|| {
        if host.windows {
            host.dir("APPDATA").map(|d| d.join("pypoetry"))
        } else if host.macos {
            host.home()
                .map(|h| h.join("Library/Application Support/pypoetry"))
        } else {
            host.dir("XDG_CONFIG_HOME")
                .or_else(|| host.home().map(|h| h.join(".config")))
                .map(|d| d.join("pypoetry"))
        }
    });
    let config = config_dir
        .and_then(|d| crate::statefile::read_capped(&d.join("config.toml"), MAX_PYPROJECT_BYTES))
        .and_then(|text| text.parse::<toml::Table>().ok());
    let setting = |path: &[&str]| -> Option<PathBuf> {
        let mut at: &toml::Value = config.as_ref()?.get(path[0])?;
        for key in &path[1..] {
            at = at.get(key)?;
        }
        Some(PathBuf::from(at.as_str()?))
    };
    let cache = host
        .dir("POETRY_CACHE_DIR")
        .or_else(|| setting(&["cache-dir"]).filter(|p| p.is_absolute()))
        .or_else(|| {
            if host.windows {
                host.dir("LOCALAPPDATA").map(|d| d.join("pypoetry/Cache"))
            } else if host.macos {
                host.home().map(|h| h.join("Library/Caches/pypoetry"))
            } else {
                host.dir("XDG_CACHE_HOME")
                    .or_else(|| host.home().map(|h| h.join(".cache")))
                    .map(|d| d.join("pypoetry"))
            }
        })?;
    if let Some(path) = setting(&["virtualenvs", "path"]) {
        let spelled = path
            .to_string_lossy()
            .replace("{cache-dir}", &cache.to_string_lossy());
        let path = PathBuf::from(spelled);
        if path.is_absolute() {
            return Some(path);
        }
    }
    Some(cache.join("virtualenvs"))
}

/// Where Pipenv keeps its environments on `host`: `WORKON_HOME`, else
/// `~/.virtualenvs` on Windows, else `$XDG_DATA_HOME/virtualenvs`, else
/// `~/.local/share/virtualenvs`.
fn pipenv_dir(host: &PythonHost) -> Option<PathBuf> {
    host.dir("WORKON_HOME").or_else(|| {
        if host.windows {
            host.home().map(|h| h.join(".virtualenvs"))
        } else {
            host.dir("XDG_DATA_HOME")
                .or_else(|| host.home().map(|h| h.join(".local/share")))
                .map(|d| d.join("virtualenvs"))
        }
    })
}

/// Hatch's environments of the project `name` on `host`
/// (`<data>/env/virtual/<name>/<id>/<env>`), the default one first.
fn hatch_envs(host: &PythonHost, name: &str) -> Vec<PathBuf> {
    let data = host.dir("HATCH_DATA_DIR").or_else(|| {
        if host.windows {
            host.dir("LOCALAPPDATA").map(|d| d.join("hatch"))
        } else if host.macos {
            host.home()
                .map(|h| h.join("Library/Application Support/hatch"))
        } else {
            host.dir("XDG_DATA_HOME")
                .or_else(|| host.home().map(|h| h.join(".local/share")))
                .map(|d| d.join("hatch"))
        }
    });
    let Some(data) = data else {
        return Vec::new();
    };
    let wanted = poetry_sanitized(name);
    let mut envs: Vec<PathBuf> = Vec::new();
    for project in sorted_dirs(&data.join("env/virtual")) {
        if poetry_sanitized(&dir_name(&project)) != wanted {
            continue;
        }
        for id in sorted_dirs(&project) {
            envs.extend(sorted_dirs(&id));
        }
    }
    // The default environment is named for the project, or `default`.
    envs.sort_by_key(|env| {
        let n = poetry_sanitized(&dir_name(env));
        !(n == wanted || n == "default")
    });
    envs
}

/// The project's uv environment as `UV_PROJECT_ENVIRONMENT` names it: a
/// folder of the project's, or one of the user's anywhere.
fn uv_project_env(root: &Path, host: &PythonHost) -> Option<PathBuf> {
    let named = PathBuf::from(host.var("UV_PROJECT_ENVIRONMENT")?);
    Some(match named.is_absolute() {
        true => named,
        false => root.join(named),
    })
}

/// The interpreter `.pdm-python` records PDM using for the project, when
/// it is one: an absolute path to a `python…` program. Read, not trusted:
/// what runs it is held to the same check as any other.
fn pdm_python(root: &Path) -> Option<PathBuf> {
    let text = crate::statefile::read_capped(&root.join(".pdm-python"), 4096)?;
    let path = PathBuf::from(text.trim());
    let named = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("python"));
    (path.is_absolute() && named).then_some(path)
}

/// The conda environment the project's `environment.yml` names
/// (`name: ml`), on `host`.
fn conda_env_named(root: &Path, host: &PythonHost) -> Option<PathBuf> {
    let text = ["environment.yml", "environment.yaml"]
        .iter()
        .find_map(|f| crate::statefile::read_capped(&root.join(f), MAX_PYPROJECT_BYTES))?;
    let name = text.lines().find_map(|line| {
        let value = line.strip_prefix("name:")?.trim();
        let value = value.trim_matches(['"', '\'']);
        (!value.is_empty() && !value.contains(['/', '\\'])).then(|| value.to_string())
    })?;
    conda_envs(host)
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, env)| env)
}

/// Where pyenv keeps its Pythons on `host`: `PYENV_ROOT`'s, else
/// `~/.pyenv`'s; pyenv-win's under `pyenv-win` in it.
fn pyenv_versions(host: &PythonHost) -> Vec<PathBuf> {
    let Some(root) = host
        .dir("PYENV_ROOT")
        .or_else(|| host.home().map(|h| h.join(".pyenv")))
    else {
        return Vec::new();
    };
    let mut dirs = sorted_dirs(&root.join("versions"));
    dirs.extend(sorted_dirs(&root.join("pyenv-win/versions")));
    dirs
}

/// Most folders above the project looked in for its `.python-version`.
const MAX_PIN_DEPTH: usize = 32;

/// The versions the project's `.python-version` pins (the one in its
/// folder or the nearest above it, as pyenv and uv read it), in order. A
/// path is no version.
fn pinned_versions(root: &Path) -> Vec<String> {
    let Some(text) = root
        .ancestors()
        .take(MAX_PIN_DEPTH)
        .find_map(|dir| crate::statefile::read_capped(&dir.join(".python-version"), 4096))
    else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(str::split_whitespace)
        .filter(|v| !v.contains(['/', '\\']) && *v != "." && *v != ".." && *v != "system")
        .map(str::to_string)
        .collect()
}

/// A version's numbers, for ordering: `3.12.4` is [3, 12, 4]; a part not a
/// number ends them.
fn version_numbers(version: &str) -> Vec<u32> {
    version
        .split('.')
        .map_while(|part| part.parse().ok())
        .collect()
}

/// Whether `version` is what `pin` asks for: the same, or a release of it
/// (`3.12` asks for `3.12.4`; `3.1` does not ask for `3.12.0`).
fn version_pinned(version: &str, pin: &str) -> bool {
    version == pin
        || version
            .strip_prefix(pin)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// The newest of `dirs` whose version (`version_of` its folder's name) is
/// what `pins` ask for, the first pin first.
fn pinned_dir(
    dirs: &[PathBuf],
    pins: &[String],
    version_of: impl Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    pins.iter().find_map(|pin| {
        dirs.iter()
            .filter_map(|dir| {
                let version = version_of(&dir_name(dir))?;
                version_pinned(&version, pin).then(|| (version_numbers(&version), dir))
            })
            .max_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, dir)| dir.clone())
    })
}

/// Conda's environments on `host`, by name: those it lists in
/// `~/.conda/environments.txt`, and the base and `envs/` of the usual
/// installs.
fn conda_envs(host: &PythonHost) -> Vec<(String, PathBuf)> {
    let Some(home) = host.home() else {
        return Vec::new();
    };
    let mut envs: Vec<(String, PathBuf)> = Vec::new();
    if let Some(list) =
        crate::statefile::read_capped(&home.join(".conda/environments.txt"), MAX_CONDA_LIST_BYTES)
    {
        for line in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let dir = PathBuf::from(line);
            if dir.is_absolute() {
                let name = match dir.parent().and_then(|p| p.file_name()) {
                    Some(parent) if parent == "envs" => dir_name(&dir),
                    _ => format!("base ({})", dir_name(&dir)),
                };
                envs.push((name, dir));
            }
        }
    }
    for install in [
        "miniconda3",
        "anaconda3",
        "miniforge3",
        "mambaforge",
        "micromamba",
        ".conda",
    ] {
        let base = home.join(install);
        envs.push((format!("base ({install})"), base.clone()));
        for env in sorted_dirs(&base.join("envs")) {
            envs.push((dir_name(&env), env));
        }
    }
    envs
}

/// What [`detect`] tells the language server of the project's Python.
struct PythonEnv {
    /// The interpreter it RUNS, to read `sys.path`.
    run: PathBuf,
    /// Directories it only reads, as extra import roots.
    extra_paths: Vec<PathBuf>,
}

/// The environment [`detect`] hands the language server: the virtualenv
/// [`find_python_interpreter`] finds, made safe to run.
///
/// Vetting the interpreter's BYTES ([`foreign_bytes`]) does not make it safe
/// to run where it is. Python starts by reading the `pyvenv.cfg` beside the
/// path it was run by, and then EXECUTES every `import` line of every `.pth`
/// file in that environment's site-packages — `-I` does not stop it. A venv
/// inside the project ships with the repository as readily as a
/// `.venv/bin/python` does, so handing pyright its interpreter ran whatever
/// those `.pth` files said, on opening a Python file.
///
/// So a venv found by name in the project is never run: the server runs the
/// base interpreter its bytes are (outside the project, booting no venv), and
/// is pointed at the venv's site-packages — and the directories its `.pth`
/// files add, read as text — as extra search paths, which it only reads.
/// What that costs is what reading cannot give: a package an `.pth` IMPORT
/// line puts on the path (an import-hook editable install, which pyright
/// cannot follow either), and a venv that hides the system's site-packages
/// does not hide them here. An activated venv (`VIRTUAL_ENV`) is the
/// environment the user chose, and is run as found.
fn find_python_env(root: &Path) -> Option<PythonEnv> {
    let found = find_vetted(root)?;
    if found.activated {
        return Some(PythonEnv {
            run: found.interpreter,
            extra_paths: Vec::new(),
        });
    }
    // `<venv>/bin/python` (or `Scripts/python.exe`) -> `<venv>`.
    let venv = found.interpreter.parent()?.parent()?.to_path_buf();
    Some(reading_env(root, found.base, site_paths(&venv)))
}

/// A venv read, not run: the server runs `base` and reads `extra_paths` —
/// as extra search paths, or, where the project's own pyright configuration
/// names its own (which replace the ones a client sends), through
/// [`interpreter_shim`]'s `sys.path`.
fn reading_env(root: &Path, base: PathBuf, extra_paths: Vec<PathBuf>) -> PythonEnv {
    let run = match !extra_paths.is_empty() && pyright_config_sets_paths(root) {
        true => interpreter_shim(&base, &extra_paths).unwrap_or(base),
        false => base,
    };
    PythonEnv { run, extra_paths }
}

/// Most bytes of a pyright configuration read.
const MAX_PYRIGHT_CONFIG_BYTES: u64 = 1 << 20;

/// Whether the project's own pyright configuration (`pyrightconfig.json`,
/// or `[tool.pyright]` in `pyproject.toml`) names its own search paths:
/// `extraPaths`, or `executionEnvironments`, each with its own. pyright then
/// takes those instead of the `python.analysis.extraPaths` a client sends,
/// and a venv's packages handed over that way are lost.
fn pyright_config_sets_paths(root: &Path) -> bool {
    const KEYS: [&str; 2] = ["extraPaths", "executionEnvironments"];
    // JSON with comments: the key, quoted, is all that is looked for.
    if let Some(text) =
        crate::statefile::read_capped(&root.join("pyrightconfig.json"), MAX_PYRIGHT_CONFIG_BYTES)
        && KEYS.iter().any(|k| text.contains(&format!("\"{k}\"")))
    {
        return true;
    }
    crate::statefile::read_capped(&root.join("pyproject.toml"), MAX_PYRIGHT_CONFIG_BYTES)
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|doc| doc.get("tool")?.get("pyright")?.as_table().cloned())
        .is_some_and(|pyright| KEYS.iter().any(|k| pyright.contains_key(*k)))
}

/// An interpreter for pyright to run in place of `base`: `base` itself,
/// isolated (`-I`: no `PYTHONPATH`, no user site, no script folder), with
/// `paths` appended to `sys.path` after its own library, so the search paths
/// pyright reads from it hold the venv's packages, and a module of theirs
/// named like one of the library's (`json.py`) is never imported in its
/// place. Nothing of the venv runs: no `pyvenv.cfg` is read, no `.pth`. A
/// shell script in clew's data directory, one per interpreter and paths;
/// `None` where there is none to write (Windows: pyright cannot run a
/// script as its interpreter).
fn interpreter_shim(base: &Path, paths: &[PathBuf]) -> Option<PathBuf> {
    if !cfg!(unix) {
        return None;
    }
    let script = shim_script(base, paths)?;
    use sha2::Digest;
    let digest = sha2::Sha256::digest(script.as_bytes());
    let name: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let root = crate::lsp::store::data_root()?.join("python-shims");
    let dir = root.join(name);
    let shim = dir.join("python");
    if std::fs::read_to_string(&shim).ok().as_deref() != Some(script.as_str()) {
        crate::lsp::store::create_store_dir(&root).ok()?;
        std::fs::create_dir_all(&dir).ok()?;
        let staged = dir.join(format!(".python.{}", std::process::id()));
        std::fs::write(&staged, &script).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).ok()?;
        }
        std::fs::rename(&staged, &shim).ok()?;
    }
    Some(shim)
}

/// The shim's text: `python -c <code>` (how pyright asks an interpreter for
/// its search paths and version) runs `<code>` after the paths are added;
/// anything else runs `base` as asked, isolated. `None` for a path that is
/// not UTF-8, which the script cannot spell.
fn shim_script(base: &Path, paths: &[PathBuf]) -> Option<String> {
    let base = base.to_str()?;
    let paths: Vec<&str> = paths.iter().map(|p| p.to_str()).collect::<Option<_>>()?;
    // A JSON list of strings is a Python list literal.
    let list = serde_json::to_string(&paths).ok()?;
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let code = [
        "import sys; sys.path.extend(",
        &list,
        "); exec(compile(sys.argv.pop(1), '<string>', 'exec'))",
    ]
    .concat();
    Some(format!(
        "#!/bin/sh\n\
         # Written by Clew: pyright's interpreter for a project whose own venv is\n\
         # read, never run. Remove it freely; Clew writes it again.\n\
         if [ \"$1\" = \"-c\" ] && [ \"$#\" -eq 2 ]; then\n\
         \x20 exec {base} -I -c {code} \"$2\"\n\
         fi\n\
         exec {base} -I \"$@\"\n",
        base = quote(base),
        code = quote(&code),
    ))
}

/// Most site-packages directories a venv is searched for.
const MAX_SITE_DIRS: usize = 8;
/// Most `.pth` files read from one site-packages directory.
const MAX_PTH_FILES: usize = 256;
/// `.pth` files are a few lines each; they ship with the repository.
const MAX_PTH_BYTES: u64 = 64 * 1024;
/// Most extra search paths handed to the server.
const MAX_EXTRA_PATHS: usize = 64;

/// The directories `venv`'s interpreter would put on `sys.path` from the venv
/// itself, found by reading rather than running it: each site-packages
/// (`lib/pythonX.Y/site-packages`, `Lib/site-packages` on Windows), then the
/// existing directories its `.pth` files list, in the order `site` adds them.
/// Their `import` lines — which `site` would execute — are skipped. Bounded
/// on every axis: the venv ships with the repository.
fn site_paths(venv: &Path) -> Vec<PathBuf> {
    let mut sites: Vec<PathBuf> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut add = |dir: PathBuf, out: &mut Vec<PathBuf>| {
        // `lib64` is often a link to `lib`: one directory, listed once.
        let Ok(real) = std::fs::canonicalize(&dir) else {
            return;
        };
        if dir.is_dir() && !seen.contains(&real) && out.len() < MAX_EXTRA_PATHS {
            seen.push(real);
            out.push(dir);
        }
    };
    for lib in ["lib", "lib64"] {
        let Ok(entries) = std::fs::read_dir(venv.join(lib)) else {
            continue;
        };
        let mut versions: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("python"))
            })
            .map(|e| e.path().join("site-packages"))
            .take(MAX_SITE_DIRS)
            .collect();
        versions.sort();
        for site in versions {
            add(site, &mut sites);
        }
    }
    add(venv.join("Lib").join("site-packages"), &mut sites);
    let mut out = sites.clone();
    for site in &sites {
        let Ok(entries) = std::fs::read_dir(site) else {
            continue;
        };
        // `site` reads them in name order.
        let mut pths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "pth"))
            .collect();
        pths.sort();
        for pth in pths.into_iter().take(MAX_PTH_FILES) {
            let Some(text) = crate::statefile::read_capped(&pth, MAX_PTH_BYTES) else {
                continue;
            };
            for line in text.lines() {
                if line.starts_with('#')
                    || line.trim().is_empty()
                    || line.starts_with("import ")
                    || line.starts_with("import\t")
                {
                    continue;
                }
                add(site.join(line.trim_end()), &mut out);
            }
        }
    }
    out
}

/// Most bytes of a `.git` file read: one `gitdir:` line.
const MAX_GITFILE_BYTES: u64 = 4096;

/// The folder everything that came with the opened folder `real_root` (already
/// canonical) lives under: the top of the working tree it belongs to, or
/// `real_root` itself outside any repository.
///
/// The opened folder alone is the wrong boundary. Opening `repo/sub` does not
/// make the rest of the clone anyone else's: `sub/.venv/bin/python ->
/// ../../tools/payload` resolves outside `sub` and inside the repository, and
/// would have passed for a system interpreter. A submodule's or linked
/// worktree's `.git` is a file naming a git directory inside the owning
/// repository's `.git`; when the owning working tree holds this one (a
/// submodule, a worktree checked out inside the main one), the boundary is
/// that whole tree, since its author wrote both.
fn repository_top(real_root: &Path) -> PathBuf {
    let Some(top) = real_root
        .ancestors()
        .find(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok())
    else {
        return real_root.to_path_buf();
    };
    let gitfile = top.join(".git");
    if gitfile.is_dir() {
        return top.to_path_buf();
    }
    let owner = crate::statefile::read_capped(&gitfile, MAX_GITFILE_BYTES).and_then(|text| {
        let gitdir = text.lines().next()?.strip_prefix("gitdir:")?.trim();
        let gitdir = std::fs::canonicalize(top.join(gitdir)).ok()?;
        // `<owner>/.git/modules/<name>` or `<owner>/.git/worktrees/<name>`.
        gitdir
            .ancestors()
            .find(|dir| dir.file_name().is_some_and(|n| n == ".git"))?
            .parent()
            .map(Path::to_path_buf)
    });
    match owner {
        Some(owner) if top.starts_with(&owner) => owner,
        _ => top.to_path_buf(),
    }
}

/// The bytes executing `cand` runs, when they came from outside `repo` (which
/// must already be canonical; see [`repository_top`]): the file outside the
/// project they are.
///
/// Two shapes pass:
///   * The real file resolves outside the project. `python -m venv` links
///     `bin/python` at the base interpreter, so the ordinary case lands here
///     and costs neither a prompt nor any accuracy.
///   * The real file is inside the project but is byte-identical to the base
///     interpreter named by the venv's own `pyvenv.cfg`, whose `home` must
///     itself resolve outside the project. A `--copies` venv is a real copy,
///     so without this arm it would be refused for being exactly what it is;
///     so is a Windows venv's interpreter, a copy of the base installation's
///     venv launcher ([`windows_venv_launcher`]).
///
/// What this prevents: a repository shipping `.venv/bin/python` as its own
/// script or binary and having clew nominate it for execution. Repo-authored
/// content fails both arms — the second compares against a file the repo does
/// not control, so a planted `pyvenv.cfg` buys nothing.
///
/// What it does NOT prevent, stated plainly rather than implied away:
///   * What the bytes do once run from the venv's own path. Python boots the
///     environment around the path it was run by and executes the `import`
///     lines of its `.pth` files, so a venv that came with the repository is
///     never run from there — [`find_python_env`] hands the language server
///     the base this returns instead. (It once read "a repo-planted symlink to
///     a system executable only chooses WHICH installed program runs": that
///     program then ran the repository's `.pth` lines.) The debugger does run
///     the venv's interpreter: it is running the project's program, at the
///     user's request, which is the same act.
///   * A real in-project interpreter with no usable `pyvenv.cfg` (`conda
///     create -p ./env`) is refused, not executed — the server falls back to
///     its own discovery. That costs accuracy, not safety.
fn foreign_bytes(repo: &Path, cand: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(cand).ok()?;
    if !real.starts_with(repo) {
        return Some(real);
    }
    copies_the_base_interpreter(repo, cand, &real)
}

/// The `--copies` arm: is the in-project `cand` a byte-for-byte copy of the
/// base interpreter its `pyvenv.cfg` points at, with that base outside the
/// project? Then executing it and executing the base are the same act.
fn copies_the_base_interpreter(repo: &Path, cand: &Path, real_cand: &Path) -> Option<PathBuf> {
    // `<venv>/bin/python` -> `<venv>`; `<venv>/Scripts/python.exe` is the same
    // shape.
    let venv = cand.parent().and_then(Path::parent)?;
    // Repository-controlled file: read it capped, and through the state-file
    // opener that refuses a symlink or a FIFO at the leaf.
    let cfg = crate::statefile::read_capped(&venv.join("pyvenv.cfg"), MAX_PYVENV_CFG_BYTES)?;
    let mut home = None;
    let mut version = None;
    for line in cfg.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "home" => home = Some(value.trim()),
            // Both spellings occur across CPython releases.
            "version" | "version_info" => version = Some(value.trim()),
            _ => {}
        }
    }
    let home = std::fs::canonicalize(home?).ok()?;
    // A `home` pointing back into the project proves nothing: the repository
    // would be vouching for itself.
    if home.starts_with(repo) {
        return None;
    }
    // `bin/python` is usually a copy of `pythonX.Y`, so the candidate's own
    // name is only one of the possibilities for the base's name.
    let mut names = Vec::new();
    if let Some(name) = cand.file_name().and_then(|n| n.to_str()) {
        names.push(name.to_string());
    }
    if let Some((major, minor)) = version.and_then(|v| {
        let mut parts = v.split('.');
        Some((parts.next()?, parts.next()?))
    }) {
        names.push(format!("python{major}.{minor}"));
    }
    names.push("python3".to_string());
    names.push("python".to_string());
    for name in names {
        let Ok(base) = std::fs::canonicalize(home.join(&name)) else {
            continue;
        };
        // The base must be outside too, or a symlink out of `home` back into
        // the project would launder the repository's own bytes.
        if base.starts_with(repo) {
            continue;
        }
        if same_bytes(real_cand, &base) {
            return Some(base);
        }
    }
    windows_venv_launcher(repo, &home, real_cand)
}

/// The Windows arm of [`copies_the_base_interpreter`]. A Windows venv's
/// `python.exe` is no copy of the base interpreter: it is the venv launcher
/// the base installation ships — `venvlauncher.exe` beside it, or
/// `Lib\venv\scripts\nt\python.exe` before Python 3.13 — which runs `home`'s
/// own `python.exe`. A byte-for-byte copy of that launcher, from a `home`
/// outside the project, runs the same bytes as the base; the base is what
/// it returns. Every Windows venv was refused for being exactly what it is.
#[cfg(windows)]
fn windows_venv_launcher(repo: &Path, home: &Path, real_cand: &Path) -> Option<PathBuf> {
    const LAUNCHERS: [&str; 3] = [
        "venvlauncher.exe",
        "Lib/venv/scripts/nt/python.exe",
        "Lib/venv/scripts/nt/venvlauncher.exe",
    ];
    let base = std::fs::canonicalize(home.join("python.exe"))
        .ok()
        .filter(|base| !base.starts_with(repo))?;
    LAUNCHERS.iter().find_map(|launcher| {
        let launcher = std::fs::canonicalize(home.join(launcher)).ok()?;
        (!launcher.starts_with(repo) && same_bytes(real_cand, &launcher)).then(|| base.clone())
    })
}

#[cfg(not(windows))]
fn windows_venv_launcher(_repo: &Path, _home: &Path, _real_cand: &Path) -> Option<PathBuf> {
    None
}

/// Do two files hold exactly the same bytes? Bounded on every axis a
/// repository controls: both are opened as plain files (never through a
/// symlink, never blocking on a FIFO), unequal or oversized lengths lose
/// before a byte is read, and the loop stops at [`MAX_INTERPRETER_BYTES`].
fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Some(mut fa), Some(mut fb)) = (
        crate::statefile::open_plain(a),
        crate::statefile::open_plain(b),
    ) else {
        return false;
    };
    let (Ok(ma), Ok(mb)) = (fa.metadata(), fb.metadata()) else {
        return false;
    };
    // An empty "interpreter" matches every other empty file, which would make
    // a zero-byte repo file pass against a zero-byte anything.
    if ma.len() == 0 || ma.len() != mb.len() || ma.len() > MAX_INTERPRETER_BYTES {
        return false;
    }
    let mut buf_a = vec![0u8; 64 * 1024];
    let mut buf_b = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let (Some(na), Some(nb)) = (fill(&mut fa, &mut buf_a), fill(&mut fb, &mut buf_b)) else {
            return false;
        };
        if na != nb || buf_a[..na] != buf_b[..nb] {
            return false;
        }
        if na == 0 {
            return true;
        }
        // Re-checked while reading: the size above races with a file that is
        // still growing, and an unbounded loop is the one failure mode this
        // must never have.
        total += na as u64;
        if total > MAX_INTERPRETER_BYTES {
            return false;
        }
    }
}

/// Read until `buf` is full or the file ends, so the two comparisons always
/// line up on the same offsets — a short read on one side must not be mistaken
/// for a difference.
fn fill(f: &mut std::fs::File, buf: &mut [u8]) -> Option<usize> {
    use std::io::Read;
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(n)
}

/// Recursively merge `over` into `base`; `over`'s values win on conflict.
fn deep_merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes standing in for a CPython binary. Never empty: an empty file
    /// would compare equal to every other empty file.
    const BASE_BYTES: &[u8] = b"\x7fELF not really python, but a distinctive body";

    /// A fresh directory for the calling test — a fixed name was shared by
    /// concurrent test runs — removed when the test ends. Scoped to the test
    /// rather than to a guard: `system_python` hands back only a path inside
    /// the base install it makes.
    fn scratch(tag: &str) -> PathBuf {
        let root = crate::testutil::scoped_dir(&format!("langenv-{tag}"));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// A repository whose `examples/` is a Cargo workspace of its own — as
    /// salvo's is — has rust-analyzer told to load it too, beside the
    /// repository's own: each workspace, and a package no workspace above it
    /// holds; not the members of either, nor what a build left in `target/`.
    /// Its files were read as part of no crate: no definition anywhere.
    #[test]
    fn nested_cargo_workspaces_are_linked() {
        let root = scratch("cargo-nested");
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
        write("crates/core/Cargo.toml", "[package]\nname = \"core\"\n");
        write("examples/Cargo.toml", "[workspace]\nmembers = [\"*\"]\n");
        write(
            "examples/otel/Cargo.toml",
            "[package]\nname = \"otel\"\nversion.workspace = true\n",
        );
        write("target/debug/build/x/Cargo.toml", "[workspace]\n");
        write("broken/Cargo.toml", "this is [not toml");
        assert_eq!(
            cargo_projects(&root),
            vec![root.join("Cargo.toml"), root.join("examples/Cargo.toml")]
        );
        let linked = detect("rust", "rust-analyzer", &root).expect("linked projects");
        assert_eq!(linked["linkedProjects"].as_array().unwrap().len(), 2);
        // Features chosen in Settings do not take the workspaces away: the
        // two are of different things, and both reach the server.
        let choices = LanguageChoices {
            rust: Some(RustChoices {
                all_features: true,
                ..RustChoices::default()
            }),
            ..LanguageChoices::default()
        };
        let server = crate::lsp::config::ProjectLspConfig::default()
            .resolve("rust")
            .unwrap();
        let exe = root.join("rust-analyzer");
        let merged = merge("rust", &server, &exe, &root, None, &choices)
            .unwrap()
            .unwrap();
        assert_eq!(merged["linkedProjects"].as_array().unwrap().len(), 2);
        assert_eq!(merged["cargo"]["features"], "all", "{merged}");

        // A repository with no manifest at its top, and packages apart in it
        // (a service and its tools): each is linked.
        let apart = scratch("cargo-apart");
        for dir in ["service", "tools/lint"] {
            std::fs::create_dir_all(apart.join(dir)).unwrap();
            std::fs::write(
                apart.join(dir).join("Cargo.toml"),
                "[package]\nname = \"x\"\n",
            )
            .unwrap();
        }
        assert_eq!(
            cargo_projects(&apart),
            vec![
                apart.join("service/Cargo.toml"),
                apart.join("tools/lint/Cargo.toml")
            ]
        );
    }

    /// The crates `cargo vendor` copied into a project — each with its
    /// checksums beside it — are its dependencies, not projects of their
    /// own, and are not linked; a package apart (`fuzz/`) still is. Beside a
    /// package at the top, every vendored crate was linked, and
    /// rust-analyzer loaded and built each as a workspace of its own.
    #[test]
    fn vendored_crates_are_not_linked() {
        let root = scratch("cargo-vendored");
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("Cargo.toml", "[package]\nname = \"app\"\n");
        for name in ["serde", "itoa"] {
            write(
                &format!("vendor/{name}/Cargo.toml"),
                &format!("[package]\nname = \"{name}\"\n"),
            );
            write(
                &format!("vendor/{name}/.cargo-checksum.json"),
                "{\"files\":{},\"package\":\"0\"}",
            );
        }
        write(
            "fuzz/Cargo.toml",
            "[package]\nname = \"app-fuzz\"\n\n[workspace]\nmembers = [\".\"]\n",
        );
        assert_eq!(
            cargo_projects(&root),
            vec![root.join("Cargo.toml"), root.join("fuzz/Cargo.toml")]
        );
    }

    /// A manifest's kind is read from its tables and top-level dotted keys,
    /// without parsing it whole — and not from a key of the same name inside
    /// another table, or a table of a longer name.
    #[test]
    fn a_manifest_is_known_by_its_tables() {
        use ManifestKind::*;
        let cases: &[(&str, Option<ManifestKind>)] = &[
            ("[package]\nname = \"a\"\n", Some(Package)),
            ("[workspace]\nmembers = [\"a\"]\n", Some(Workspace)),
            ("[package]\nname = \"a\"\n\n[workspace]\n", Some(Workspace)),
            ("[workspace.dependencies]\nserde = \"1\"\n", Some(Workspace)),
            ("  [ workspace ]\n", Some(Workspace)),
            ("workspace.members = [\"a\"]\n", Some(Workspace)),
            ("package.name = \"a\"\n", Some(Package)),
            ("[dependencies]\nworkspace = true\n", None),
            ("[dependencies]\nserde.workspace = true\n", None),
            ("[[bin]]\nname = \"workspace\"\n", None),
            ("[packages]\n", None),
            ("[workspaces]\n", None),
            ("# [workspace]\n[lib]\n", None),
            ("", None),
        ];
        for (manifest, kind) in cases {
            assert_eq!(manifest_kind(manifest), *kind, "{manifest:?}");
        }
    }

    /// However many packages a tree holds apart, no more than
    /// CARGO_MAX_PROJECTS are linked: rust-analyzer could not load each in
    /// any case, and reading every manifest held the window still.
    #[test]
    fn a_tree_of_many_packages_links_a_bounded_few() {
        let root = scratch("cargo-many");
        for n in 0..CARGO_MAX_PROJECTS + 10 {
            let dir = root.join(format!("crates/c{n:03}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname = \"c{n}\"\n"),
            )
            .unwrap();
        }
        let projects = cargo_projects(&root);
        assert_eq!(projects.len(), CARGO_MAX_PROJECTS);
        assert_eq!(projects[0], root.join("crates/c000/Cargo.toml"));
    }

    /// A project of one workspace at its top is left to rust-analyzer, which
    /// finds that itself.
    #[test]
    fn one_workspace_at_the_top_needs_no_linking() {
        let root = scratch("cargo-single");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"one\"\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        assert_eq!(cargo_projects(&root), vec![root.join("Cargo.toml")]);
        assert_eq!(detect("rust", "rust-analyzer", &root), None);
    }

    /// A "system Python" outside any project root, returning its `bin`
    /// directory (what `pyvenv.cfg`'s `home` names).
    fn system_python(tag: &str, bytes: &[u8]) -> PathBuf {
        let bin = scratch(tag).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("python3.12"), bytes).unwrap();
        bin
    }

    /// What the detector picks *inside* `root`. An ambient `VIRTUAL_ENV` on
    /// the developer's machine is a real hit but not the one under test, so
    /// it is filtered out rather than allowed to decide these assertions.
    fn project_local(root: &Path) -> Option<PathBuf> {
        find_python_interpreter(root).filter(|p| p.starts_with(root))
    }

    fn venv_bin(root: &Path, dir: &str) -> PathBuf {
        let bin = root.join(dir).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        bin
    }

    /// The ordinary case: `python -m venv` links `bin/python` at the base
    /// interpreter, so the real bytes live outside the project and the
    /// interpreter is detected with no prompt and no loss of accuracy.
    #[test]
    #[cfg(unix)]
    fn finds_symlinked_venv_interpreter() {
        let root = scratch("venv");
        let base = system_python("venv-base", BASE_BYTES);
        let bin = venv_bin(&root, ".venv");
        std::os::unix::fs::symlink(base.join("python3.12"), bin.join("python")).unwrap();
        let found = project_local(&root).unwrap();
        assert!(found.ends_with(".venv/bin/python"), "{found:?}");
    }

    /// …and the same claim against a venv that `python -m venv` really built,
    /// so "a normal project still resolves, with no new prompt" does not rest
    /// on a hand-made fixture. It needs a `python3` that can build one, as
    /// other tests need `git`: without it this fails, rather than passing
    /// having checked nothing.
    #[test]
    fn finds_a_real_python_venv() {
        let root = scratch("realvenv");
        // By absolute path, never a bare name resolved against every PATH
        // entry.
        // (Windows' Python is `python`; its `python3`, where there is one, is
        // the Store's stand-in.)
        let names: &[&str] = if cfg!(windows) {
            &["python", "python3"]
        } else {
            &["python3"]
        };
        let python3 = names
            .iter()
            .find_map(|name| crate::lsp::store::find_on_path_unresolved(name))
            .expect("this test needs python3 on PATH");
        let built = std::process::Command::new(python3)
            .args(["-m", "venv", "--without-pip", ".venv"])
            .current_dir(&root)
            .output()
            .expect("python3 runs");
        assert!(
            built.status.success(),
            "python3 could not build a venv: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        let found = project_local(&root).expect("a real venv must still resolve");
        assert!(found.starts_with(root.join(".venv")), "{found:?}");
    }

    /// R11-07: a repository can ship `.venv/bin/python` as its own file, and
    /// pyright RUNS the interpreter it is handed. A real in-project file that
    /// is nobody's copy is refused, so nothing nominates it for execution.
    #[test]
    fn rejects_repo_shipped_interpreter() {
        // An activated virtualenv (`VIRTUAL_ENV`) is a candidate of its own:
        // the developer's shell, or a test that sets one, must not answer
        // for the project. Held for the whole test, under the env lock.
        let _env = crate::testutil::EnvVars::new().remove("VIRTUAL_ENV");
        let root = scratch("hostile");
        let bin = venv_bin(&root, ".venv");
        std::fs::write(bin.join("python"), "#!/bin/sh\ntouch /tmp/pwned\n").unwrap();
        assert_eq!(project_local(&root), None);
        assert!(detect("python", "pyright", &root).is_none());
    }

    /// A `--copies` venv (and every Windows venv) holds a real copy of the
    /// base interpreter. It must still resolve, or the check would refuse a
    /// legitimate environment for being what it is.
    #[test]
    fn accepts_copies_venv_matching_its_base() {
        let root = scratch("copies");
        let base = system_python("copies-base", BASE_BYTES);
        let bin = venv_bin(&root, ".venv");
        std::fs::write(bin.join("python"), BASE_BYTES).unwrap();
        std::fs::write(
            root.join(".venv/pyvenv.cfg"),
            format!("home = {}\nversion = 3.12.1\n", base.display()),
        )
        .unwrap();
        let found = project_local(&root).unwrap();
        assert!(found.ends_with(".venv/bin/python"), "{found:?}");
    }

    /// The copy arm compares BYTES, so a `pyvenv.cfg` that points at a real
    /// system Python does not vouch for a payload sitting next to it.
    #[test]
    fn rejects_copies_venv_that_differs_from_its_base() {
        let root = scratch("forged");
        let base = system_python("forged-base", BASE_BYTES);
        let bin = venv_bin(&root, ".venv");
        std::fs::write(bin.join("python"), b"#!/bin/sh\ntouch /tmp/pwned\n").unwrap();
        std::fs::write(
            root.join(".venv/pyvenv.cfg"),
            format!("home = {}\nversion = 3.12.1\n", base.display()),
        )
        .unwrap();
        assert_eq!(project_local(&root), None);
    }

    /// …and a `home` pointing back into the project is the repository
    /// vouching for itself, byte-equal or not.
    #[test]
    fn rejects_pyvenv_cfg_home_inside_the_project() {
        let root = scratch("selfhome");
        let fake_base = root.join("tools");
        std::fs::create_dir_all(&fake_base).unwrap();
        std::fs::write(fake_base.join("python3.12"), BASE_BYTES).unwrap();
        let bin = venv_bin(&root, ".venv");
        std::fs::write(bin.join("python"), BASE_BYTES).unwrap();
        std::fs::write(
            root.join(".venv/pyvenv.cfg"),
            format!("home = {}\nversion = 3.12.1\n", fake_base.display()),
        )
        .unwrap();
        assert_eq!(project_local(&root), None);
    }

    /// Opening a subfolder does not make the rest of the clone foreign: a link
    /// out of the opened folder into another folder of the same repository —
    /// or of the superproject a submodule sits in — is the repository's own
    /// file, and is refused like one.
    #[test]
    #[cfg(unix)]
    fn rejects_a_link_into_the_rest_of_the_repository() {
        let _env = crate::testutil::EnvVars::new().remove("VIRTUAL_ENV");
        let repo = scratch("subfolder");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("tools")).unwrap();
        std::fs::write(repo.join("tools/payload"), "#!/bin/sh\ntouch /tmp/pwned\n").unwrap();
        let sub = repo.join("sub");
        let bin = venv_bin(&sub, ".venv");
        std::os::unix::fs::symlink("../../../tools/payload", bin.join("python")).unwrap();
        assert!(bin.join("python").is_file(), "the link resolves");
        assert_eq!(project_local(&sub), None);

        // The same opened folder as a submodule: its `.git` is a file naming
        // a git directory inside the superproject's.
        std::fs::remove_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join(".git/modules/sub")).unwrap();
        std::fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").unwrap();
        assert_eq!(
            repository_top(&std::fs::canonicalize(&sub).unwrap()),
            std::fs::canonicalize(&repo).unwrap()
        );
        assert_eq!(project_local(&sub), None);

        // A linked worktree checked out elsewhere is bounded by its own tree.
        let main = scratch("worktree-main");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        let wt = scratch("worktree-linked");
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        let wt = std::fs::canonicalize(&wt).unwrap();
        assert_eq!(repository_top(&wt.join("pkg")), wt);
    }

    /// Zero bytes equal zero bytes: without the length guard an empty repo
    /// file would "match" any empty file in a plausible `home`.
    #[test]
    fn rejects_empty_interpreter_matching_empty_base() {
        let root = scratch("empties");
        let base = system_python("empties-base", b"");
        let bin = venv_bin(&root, ".venv");
        std::fs::write(bin.join("python"), b"").unwrap();
        std::fs::write(
            root.join(".venv/pyvenv.cfg"),
            format!("home = {}\nversion = 3.12.1\n", base.display()),
        )
        .unwrap();
        assert_eq!(project_local(&root), None);
    }

    /// With no project venv and no activated one, there is nothing for
    /// `detect` to add at all — checked with `VIRTUAL_ENV` cleared for the
    /// test, not skipped when the developer's shell has one active. An
    /// activated venv outside the project is then picked up the same way.
    #[test]
    #[cfg(unix)]
    fn no_interpreter_no_settings() {
        let root = scratch("empty");
        assert_eq!(project_local(&root), None);
        let env = crate::testutil::EnvVars::new().remove("VIRTUAL_ENV");
        assert!(detect("python", "pyright", &root).is_none());

        let base = system_python("activated-base", BASE_BYTES);
        let active = scratch("activated");
        let bin = venv_bin(&active, "venv");
        std::os::unix::fs::symlink(base.join("python3.12"), bin.join("python")).unwrap();
        let _env = env.set("VIRTUAL_ENV", active.join("venv"));
        let opts = detect("python", "pyright", &root).expect("the activated venv");
        assert_eq!(
            opts["python"]["pythonPath"].as_str(),
            Some(bin.join("python").to_str().unwrap())
        );
    }

    #[test]
    #[cfg(unix)]
    fn detect_produces_python_path_keys() {
        let _env = crate::testutil::EnvVars::new().remove("VIRTUAL_ENV");
        let root = scratch("keys");
        let base = system_python("keys-base", BASE_BYTES);
        let bin = venv_bin(&root, "venv");
        std::os::unix::fs::symlink(base.join("python3.12"), bin.join("python3")).unwrap();
        let opts = detect("python", "pyright", &root).unwrap();
        let base_real = std::fs::canonicalize(base.join("python3.12")).unwrap();
        assert_eq!(
            opts["python"]["pythonPath"].as_str(),
            base_real.to_str(),
            "the base is run, not the venv the project ships"
        );
        assert!(opts["python"]["defaultInterpreterPath"].is_string());
    }

    /// A venv in the project ships with the repository, and Python RUNS what
    /// it says: started by the venv's own path, it reads the `pyvenv.cfg`
    /// there and executes every `import` line of its site-packages' `.pth`
    /// files, `-I` or not. So the server is handed the base interpreter —
    /// outside the project, booting no venv — and the venv's packages as
    /// search paths it only reads: its site-packages, and the directories its
    /// `.pth` files list. The `import` lines are skipped, never run.
    #[test]
    #[cfg(unix)]
    fn a_project_venv_is_read_never_run() {
        let _env = crate::testutil::EnvVars::new().remove("VIRTUAL_ENV");
        let root = scratch("pth");
        let base = system_python("pth-base", BASE_BYTES);
        let bin = venv_bin(&root, ".venv");
        std::os::unix::fs::symlink(base.join("python3.12"), bin.join("python")).unwrap();
        std::fs::write(
            root.join(".venv/pyvenv.cfg"),
            format!("home = {}\n", base.display()),
        )
        .unwrap();
        let site = root.join(".venv/lib/python3.12/site-packages");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::create_dir_all(root.join("editable/src")).unwrap();
        std::fs::write(
            site.join("planted.pth"),
            "# a comment\nimport os; os.system('touch /tmp/pwned')\n../../../../editable/src\nmissing/dir\n",
        )
        .unwrap();

        let opts = detect("python", "pyright", &root).expect("the venv is still used");
        let run = opts["python"]["pythonPath"].as_str().unwrap();
        assert!(!Path::new(run).starts_with(&root), "{run}");
        let extra: Vec<&str> = opts["python"]["analysis"]["extraPaths"]
            .as_array()
            .expect("the venv's packages are search paths")
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect();
        assert_eq!(extra.len(), 2, "{extra:?}");
        assert_eq!(extra[0], site.to_str().unwrap());
        assert!(extra[1].ends_with("editable/src"), "{extra:?}");
        // The debugger still runs the venv: it is running the project.
        assert_eq!(project_local(&root), Some(bin.join("python")));
    }

    /// A project whose own pyright configuration names its search paths
    /// (which replace the ones a client sends) has its venv's packages put
    /// on the path pyright reads from the interpreter instead: the base,
    /// through a shim in clew's data directory. Without one the extra paths
    /// stay the way they are handed over.
    #[test]
    #[cfg(unix)]
    fn a_pyright_config_with_its_own_paths_gets_the_venv_through_a_shim() {
        let data = scratch("shim-data");
        let _env = crate::testutil::EnvVars::new()
            .remove("VIRTUAL_ENV")
            .set("CLEW_DATA_DIR", &data);
        let root = scratch("shim");
        let base = system_python("shim-base", BASE_BYTES);
        let bin = venv_bin(&root, ".venv");
        std::os::unix::fs::symlink(base.join("python3.12"), bin.join("python")).unwrap();
        std::fs::write(
            root.join(".venv/pyvenv.cfg"),
            format!("home = {}\n", base.display()),
        )
        .unwrap();
        let site = root.join(".venv/lib/python3.12/site-packages");
        std::fs::create_dir_all(&site).unwrap();
        let run = |root: &Path| {
            let opts = detect("python", "pyright", root).unwrap();
            PathBuf::from(opts["python"]["pythonPath"].as_str().unwrap())
        };
        // No configuration of its own: the base, as before.
        assert_eq!(run(&root), base.join("python3.12"));
        std::fs::write(
            root.join("pyproject.toml"),
            "[tool.pyright]\nextraPaths = [\"src\"]\n",
        )
        .unwrap();
        let shim = run(&root);
        assert!(shim.starts_with(&data), "{}", shim.display());
        let text = std::fs::read_to_string(&shim).unwrap();
        assert!(
            text.contains(&format!("'{}' -I", base.join("python3.12").display())),
            "{text}"
        );
        assert!(text.contains(site.to_str().unwrap()), "{text}");
        // The same shim the next time: written once.
        assert_eq!(run(&root), shim);
        std::fs::remove_file(root.join("pyproject.toml")).unwrap();
        std::fs::write(
            root.join("pyrightconfig.json"),
            "{\n  // ours\n  \"executionEnvironments\": [{\"root\": \"src\"}]\n}\n",
        )
        .unwrap();
        assert_eq!(run(&root), shim);
        std::fs::write(
            root.join("pyrightconfig.json"),
            "{\"typeCheckingMode\": \"strict\"}",
        )
        .unwrap();
        assert_eq!(run(&root), base.join("python3.12"));
    }

    /// The shim runs the real base isolated, the venv's packages after its
    /// own library: what pyright reads holds them, and a venv module named
    /// like the library's (`json.py`) is never what is imported.
    #[test]
    #[cfg(unix)]
    fn the_shim_adds_the_venvs_packages_after_the_library() {
        let Some(python) = crate::lsp::store::find_on_path_unresolved("python3") else {
            return;
        };
        let dir = scratch("shim-run");
        let site = dir.join("it's site-packages");
        std::fs::create_dir_all(&site).unwrap();
        let planted = dir.join("planted");
        std::fs::write(
            site.join("json.py"),
            format!("open({:?}, 'w').close()\n", planted.to_str().unwrap()),
        )
        .unwrap();
        let shim = dir.join("python");
        std::fs::write(
            &shim,
            shim_script(&python, std::slice::from_ref(&site)).unwrap(),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = std::process::Command::new(&shim)
            .args(["-c", "import sys, json; json.dump(sys.path, sys.stdout)"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let path: Vec<String> = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(path.last().map(String::as_str), site.to_str());
        assert!(!planted.exists(), "the venv's json.py ran");
    }

    /// typescript-language-server runs the project's own TypeScript when it
    /// finds one, so clew points it at the copy installed beside it — and
    /// refuses to start it when that copy is missing, since a path that does
    /// not exist sends the server back to the project's.
    #[test]
    fn vue_and_astro_run_clews_own_typescript() {
        let config = crate::lsp::config::ProjectLspConfig::default();
        let vue = config.resolve("vue").unwrap();
        let install = scratch("vueinstall");
        let exe = install.join("node_modules/.bin/typescript-language-server");
        std::fs::create_dir_all(install.join("node_modules/typescript/lib")).unwrap();
        std::fs::write(install.join("node_modules/typescript/lib/tsserver.js"), "").unwrap();
        let err = pinned(&vue, &exe).unwrap_err();
        assert!(err.contains("Vue plugin is missing"), "{err}");
        std::fs::create_dir_all(install.join("node_modules/@vue/typescript-plugin")).unwrap();
        let opts = pinned(&vue, &exe).unwrap().unwrap();
        assert_eq!(
            Path::new(opts["tsserver"]["path"].as_str().unwrap()),
            install.join("node_modules/typescript/lib/tsserver.js")
        );
        assert_eq!(opts["plugins"][0]["name"], "@vue/typescript-plugin");
        assert_eq!(
            Path::new(opts["plugins"][0]["location"].as_str().unwrap()),
            install,
            "where tsserver resolves the plugin from"
        );
        assert_eq!(opts["plugins"][0]["languages"], json!(["vue"]));
        assert_eq!(opts["preferences"]["includeInlayVariableTypeHints"], true);

        let astro = config.resolve("astro").unwrap();
        let install = scratch("astroinstall");
        let exe = install.join("node_modules/.bin/astro-ls");
        assert!(pinned(&astro, &exe).is_err(), "no TypeScript of clew's");
        std::fs::create_dir_all(install.join("node_modules/typescript/lib")).unwrap();
        let opts = pinned(&astro, &exe).unwrap().unwrap();
        assert_eq!(
            Path::new(opts["typescript"]["tsdk"].as_str().unwrap()),
            install.join("node_modules/typescript/lib")
        );
        assert_eq!(
            detect("typescript", "deno", &install),
            Some(json!({ "enable": true, "lint": true }))
        );
        assert_eq!(
            detect("typescript", "typescript-language-server", &install),
            None
        );
    }

    #[test]
    fn typescript_runs_clews_own_tsserver() {
        let server = crate::lsp::config::ProjectLspConfig::default()
            .resolve("typescript")
            .unwrap();
        let install = scratch("tsinstall");
        let exe = install.join("node_modules/.bin/typescript-language-server");
        let err = pinned(&server, &exe).unwrap_err();
        assert!(err.contains("TypeScript is missing"), "{err}");

        let lib = install.join("node_modules/typescript/lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("tsserver.js"), "// clew's own").unwrap();
        let tsserver = lib.join("tsserver.js");
        let opts = pinned(&server, &exe).unwrap().unwrap();
        assert_eq!(
            Path::new(opts["tsserver"]["path"].as_str().unwrap()),
            tsserver
        );

        // Merged beneath everything: other options keep it…
        let root = scratch("tsproject");
        let kept = merge(
            "typescript",
            &server,
            &exe,
            &root,
            Some(json!({ "preferences": { "quotePreference": "single" } })),
            &LanguageChoices::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            Path::new(kept["tsserver"]["path"].as_str().unwrap()),
            tsserver
        );
        assert_eq!(kept["preferences"]["quotePreference"], "single");
        // …and an approved `tsserver.path` of the user's own still wins.
        let chosen = merge(
            "typescript",
            &server,
            &exe,
            &root,
            Some(json!({ "tsserver": { "path": "/mine" } })),
            &LanguageChoices::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(chosen["tsserver"]["path"], "/mine");

        // A server with nothing to pin, and a `command`, get nothing.
        let python = crate::lsp::config::ProjectLspConfig::default()
            .resolve("python")
            .unwrap();
        assert_eq!(pinned(&python, &exe).unwrap(), None);
        let mut custom = server.clone();
        custom.command = Some(PathBuf::from("/opt/tls"));
        assert_eq!(pinned(&custom, &exe).unwrap(), None);
    }

    /// The Python server the registry resolves to, and a stand-in binary.
    #[cfg(unix)]
    fn python_server() -> (crate::lsp::config::EffectiveServer, PathBuf) {
        let server = crate::lsp::config::ProjectLspConfig::default()
            .resolve("python")
            .unwrap();
        (
            server,
            PathBuf::from("/store/pyright/bin/pyright-langserver"),
        )
    }

    #[cfg(unix)]
    fn choose(python: &Path) -> LanguageChoices {
        LanguageChoices {
            python: Some(python.to_string_lossy().into_owned()),
            ..Default::default()
        }
    }

    /// The user's choice wins over the project's `lsp.toml` and over the venv
    /// detection finds — and stands alone: none of the detected venv's search
    /// paths linger under it. A choice that is gone is passed over, and the
    /// `lsp.toml` and detection decide as before.
    #[test]
    #[cfg(unix)]
    fn a_chosen_interpreter_wins_over_lsp_toml_and_detection() {
        let root = scratch("chosen-wins");
        let base = system_python("chosen-wins-base", BASE_BYTES);
        let bin = venv_bin(&root, ".venv");
        std::os::unix::fs::symlink(base.join("python3.12"), bin.join("python")).unwrap();
        std::fs::create_dir_all(root.join(".venv/lib/python3.12/site-packages")).unwrap();
        let mine = system_python("chosen-wins-mine", b"\x7fELF the user's own");
        let mine = mine.join("python3.12");
        let (server, exe) = python_server();
        let lsp_toml = Some(json!({ "python": { "pythonPath": "/team/python" } }));

        let merged = merge(
            "python",
            &server,
            &exe,
            &root,
            lsp_toml.clone(),
            &choose(&mine),
        )
        .unwrap()
        .unwrap();
        assert_eq!(merged["python"]["pythonPath"], mine.to_str().unwrap());
        assert_eq!(
            merged["python"]["defaultInterpreterPath"],
            mine.to_str().unwrap()
        );
        assert!(
            merged["python"].get("analysis").is_none(),
            "the detected venv's paths stayed: {merged}"
        );

        let gone = choose(&root.join("gone/bin/python"));
        let merged = merge("python", &server, &exe, &root, lsp_toml, &gone)
            .unwrap()
            .unwrap();
        assert_eq!(merged["python"]["pythonPath"], "/team/python");
        let merged = merge("python", &server, &exe, &root, None, &gone)
            .unwrap()
            .unwrap();
        assert_eq!(
            merged["python"]["pythonPath"],
            std::fs::canonicalize(base.join("python3.12"))
                .unwrap()
                .to_str()
                .unwrap(),
            "detection's pick, run through its base"
        );
    }

    /// A choice inside the repository does not lift the boundary detection
    /// draws: a venv there is run through its base interpreter, its packages
    /// read (the debugger runs it as found); a file of the repository's own
    /// is not used at all, by the server or the debugger, and is reported as
    /// such.
    #[test]
    #[cfg(unix)]
    fn a_chosen_interpreter_in_the_repository_is_held_to_detections_check() {
        let root = scratch("chosen-inside");
        std::fs::create_dir(root.join(".git")).unwrap();
        let base = system_python("chosen-inside-base", BASE_BYTES);
        let bin = venv_bin(&root, "envs/api");
        std::os::unix::fs::symlink(base.join("python3.12"), bin.join("python")).unwrap();
        let site = root.join("envs/api/lib/python3.12/site-packages");
        std::fs::create_dir_all(&site).unwrap();
        let venv_python = bin.join("python");

        let env = chosen_python_env(&root, venv_python.to_str().unwrap()).unwrap();
        assert_eq!(
            env.run,
            std::fs::canonicalize(base.join("python3.12")).unwrap()
        );
        assert_eq!(env.extra_paths, [site]);
        assert_eq!(
            chosen_interpreter(&root, venv_python.to_str().unwrap()),
            Some(venv_python.clone())
        );

        let planted = venv_bin(&root, "tools").join("python");
        std::fs::write(&planted, "#!/bin/sh\necho pwned\n").unwrap();
        let planted = planted.to_str().unwrap();
        assert!(chosen_python_env(&root, planted).is_none());
        assert_eq!(chosen_interpreter(&root, planted), None);
        let effect = python_in_effect(
            &root,
            &LanguageChoices {
                python: Some(planted.to_string()),
                ..Default::default()
            },
            None,
        );
        assert_eq!(effect.unusable_choice.as_deref(), Some(planted));
        assert_ne!(effect.source, PythonSource::Chosen);

        // Not a path at all, or a relative one, is not used either.
        assert!(chosen_python_env(&root, "python3").is_none());
        assert!(chosen_python_env(&root, "").is_none());
    }

    /// What the view says of the interpreter in effect: who named it, in the
    /// order `merge` layers them.
    #[test]
    #[cfg(unix)]
    fn what_is_in_effect_says_who_named_it() {
        let _env = crate::testutil::EnvVars::new().remove("VIRTUAL_ENV");
        let root = scratch("in-effect");
        let none = LanguageChoices::default();
        assert_eq!(
            python_in_effect(&root, &none, None),
            PythonInEffect::default()
        );

        let base = system_python("in-effect-base", BASE_BYTES);
        let bin = venv_bin(&root, ".venv");
        std::os::unix::fs::symlink(base.join("python3.12"), bin.join("python")).unwrap();
        let venv = bin.join("python").to_string_lossy().into_owned();
        let effect = python_in_effect(&root, &none, None);
        assert_eq!(
            (effect.interpreter.as_deref(), effect.source),
            (Some(venv.as_str()), PythonSource::ProjectVenv)
        );

        let lsp_toml = json!({ "python": { "defaultInterpreterPath": "/team/python" } });
        let effect = python_in_effect(&root, &none, Some(&lsp_toml));
        assert_eq!(
            (effect.interpreter.as_deref(), effect.source),
            (Some("/team/python"), PythonSource::LspToml)
        );

        let mine = system_python("in-effect-mine", b"\x7fELF mine").join("python3.12");
        let effect = python_in_effect(&root, &choose(&mine), Some(&lsp_toml));
        assert_eq!(effect.source, PythonSource::Chosen);
        assert_eq!(effect.interpreter.as_deref(), mine.to_str());
        assert_eq!(effect.unusable_choice, None);

        let effect = python_in_effect(&root, &choose(Path::new("/gone/python")), None);
        assert_eq!(effect.source, PythonSource::ProjectVenv);
        assert_eq!(effect.unusable_choice.as_deref(), Some("/gone/python"));
    }

    /// Every tool's environments are found where it keeps them — none run —
    /// the project's own first, and a planted interpreter in the project is
    /// not offered as one.
    #[test]
    #[cfg(unix)]
    fn every_tools_environments_are_found_where_it_keeps_them() {
        use std::os::unix::fs::symlink;
        let home = scratch("candidates-home");
        let root = scratch("candidates-project").join("my-app");
        std::fs::create_dir_all(&root).unwrap();
        let base = system_python("candidates-base", BASE_BYTES).join("python3.12");
        let python_at = |dir: &Path, exe: &str| {
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            std::fs::write(dir.join("bin").join(exe), "").unwrap();
            dir.join("bin").join(exe)
        };
        // The project's own: linked out (listed), planted (not).
        symlink(&base, venv_bin(&root, ".venv").join("python")).unwrap();
        std::fs::write(
            root.join(".venv/pyvenv.cfg"),
            "home = /x\nversion = 3.12.1\n",
        )
        .unwrap();
        symlink(&base, venv_bin(&root, "backend/.venv").join("python")).unwrap();
        std::fs::write(venv_bin(&root, "venv").join("python"), "#!/bin/sh\n").unwrap();
        std::fs::write(
            root.join("pyproject.toml"),
            "[tool.poetry]\nname = \"My App\"\n",
        )
        .unwrap();
        std::fs::write(root.join(".python-version"), "3.12.4\n").unwrap();
        // Activated, Poetry, Pipenv, conda, pyenv, uv, PATH.
        let active = home.join("active-env");
        python_at(&active, "python");
        let cache = home.join("cache");
        let poetry = cache.join("pypoetry/virtualenvs");
        python_at(&poetry.join("my_app-AbCd1234-py3.12"), "python");
        std::fs::write(
            poetry.join("my_app-AbCd1234-py3.12/pyvenv.cfg"),
            "version_info = 3.12.2\n",
        )
        .unwrap();
        python_at(&poetry.join("other-XyZ-py3.11"), "python");
        let workon = home.join("workon");
        python_at(&workon.join("my-app-Q1w2E3r4"), "python");
        let conda = home.join("miniconda3");
        python_at(&conda, "python");
        python_at(&conda.join("envs/ml"), "python");
        std::fs::create_dir_all(conda.join("envs/ml/conda-meta")).unwrap();
        std::fs::write(
            conda.join("envs/ml/conda-meta/python-3.11.5-h1234.json"),
            "{}",
        )
        .unwrap();
        let pyenv = home.join("pyenv");
        python_at(&pyenv.join("versions/3.11.9"), "python");
        python_at(&pyenv.join("versions/3.12.4"), "python");
        let uv = home.join("uv");
        python_at(&uv.join("cpython-3.13.0-macos-aarch64-none"), "python3");
        let on_path = home.join("usr/bin");
        std::fs::create_dir_all(&on_path).unwrap();
        std::fs::write(on_path.join("python3"), "").unwrap();

        let host = PythonHost {
            home: Some(home.clone()),
            path: Some(std::env::join_paths([&on_path]).unwrap()),
            vars: vec![
                ("VIRTUAL_ENV", active.clone().into_os_string()),
                ("XDG_CACHE_HOME", cache.clone().into_os_string()),
                ("WORKON_HOME", workon.clone().into_os_string()),
                ("PYENV_ROOT", pyenv.clone().into_os_string()),
                ("UV_PYTHON_INSTALL_DIR", uv.clone().into_os_string()),
            ],
            macos: false,
            windows: false,
        };
        let found: Vec<(PythonEnvKind, String, Option<String>)> = python_candidates(&root, &host)
            .into_iter()
            .filter(|c| {
                c.kind != PythonEnvKind::System || c.interpreter.starts_with(home.to_str().unwrap())
            })
            .map(|c| (c.kind, c.name, c.version))
            .collect();
        let v = |s: &str| Some(s.to_string());
        assert_eq!(
            found,
            [
                (PythonEnvKind::Project, ".venv".into(), v("3.12.1")),
                (PythonEnvKind::Project, "backend/.venv".into(), None),
                (PythonEnvKind::Activated, "active-env".into(), None),
                (
                    PythonEnvKind::Poetry,
                    "my_app-AbCd1234-py3.12".into(),
                    v("3.12.2")
                ),
                (PythonEnvKind::Pipenv, "my-app-Q1w2E3r4".into(), None),
                (PythonEnvKind::Conda, "base (miniconda3)".into(), None),
                (PythonEnvKind::Conda, "ml".into(), v("3.11.5")),
                (PythonEnvKind::Pyenv, "3.12.4".into(), v("3.12.4")),
                (PythonEnvKind::Pyenv, "3.11.9".into(), v("3.11.9")),
                (
                    PythonEnvKind::Uv,
                    "cpython-3.13.0-macos-aarch64-none".into(),
                    v("3.13.0")
                ),
                (
                    PythonEnvKind::System,
                    on_path.join("python3").to_string_lossy().into_owned(),
                    None
                ),
            ]
        );
    }

    /// Poetry and Pipenv name a project's environment by its name and a hash
    /// of where it is, as here: checked against what Poetry made.
    #[test]
    fn a_projects_environment_is_named_as_its_tool_names_it() {
        assert_eq!(path_hash(b"/home/me/Demo Proj", 32), "DzMhrzig");
        assert_eq!(path_hash(b"/home/me/demo/Pipfile", 6), "W2e2KVS-");
    }

    /// With no virtualenv in the project and none activated, the project's
    /// own environment as its tool keeps it is used: its Poetry one (the
    /// Python Poetry has in use, else the newest), its Pipenv one, or the
    /// pyenv Python it pins — never one the repository names by a path.
    #[test]
    fn this_projects_tool_environment_is_found_where_its_tool_keeps_it() {
        let python_at = |dir: &Path, exe: &str| {
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            std::fs::write(dir.join("bin").join(exe), "").unwrap();
        };
        let home = scratch("managed-home");
        let root = scratch("managed-root").join("Demo Proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("pyproject.toml"),
            "[tool.poetry]\nname = \"Demo App\"\n",
        )
        .unwrap();
        let real = std::fs::canonicalize(&root).unwrap();
        let cache = home.join("cache");
        let poetry = cache.join("pypoetry/virtualenvs");
        let name = format!(
            "demo_app-{}",
            path_hash(real.to_string_lossy().as_bytes(), 32)
        );
        for py in ["3.9", "3.12", "3.11"] {
            python_at(&poetry.join(format!("{name}-py{py}")), "python");
        }
        // Another project's of the same name: not this one's.
        python_at(&poetry.join("demo_app-AAAAAAAA-py3.13"), "python");
        let host = |vars: Vec<(&'static str, OsString)>| PythonHost {
            home: Some(home.clone()),
            path: None,
            vars,
            macos: false,
            windows: false,
        };
        let cached = || vec![("XDG_CACHE_HOME", cache.clone().into_os_string())];
        let found = |host: &PythonHost| {
            find_vetted_on(&root, host).map(|v| (v.interpreter, v.managed, v.activated))
        };
        let python = |env: &Path| Some(env.join("bin/python"));
        assert_eq!(
            found(&host(cached())),
            Some((
                python(&poetry.join(format!("{name}-py3.12"))).unwrap(),
                Some(PythonEnvKind::Poetry),
                true
            )),
            "the newest: 3.12, not 3.9"
        );
        std::fs::write(
            poetry.join("envs.toml"),
            format!("[{name}]\nminor = \"3.11\"\npatch = \"3.11.4\"\n"),
        )
        .unwrap();
        assert_eq!(
            found(&host(cached())).map(|f| f.0),
            python(&poetry.join(format!("{name}-py3.11"))),
            "the one Poetry has in use"
        );
        // One activated comes first.
        let active = home.join("active-env");
        python_at(&active, "python");
        let mut vars = cached();
        vars.push(("VIRTUAL_ENV", active.clone().into_os_string()));
        assert_eq!(
            found(&host(vars)),
            Some((python(&active).unwrap(), None, true))
        );
        // Conda's `base`, active in every shell by default, comes after the
        // project's own; a conda environment the user activated, before.
        let conda = home.join("miniconda3");
        python_at(&conda, "python");
        let in_conda = |env: &str| {
            let mut vars = cached();
            vars.push(("CONDA_PREFIX", conda.clone().into_os_string()));
            vars.push(("CONDA_DEFAULT_ENV", env.into()));
            found(&host(vars)).map(|f| f.0)
        };
        assert_eq!(
            in_conda("base"),
            python(&poetry.join(format!("{name}-py3.11")))
        );
        assert_eq!(in_conda("ml"), python(&conda));
        // Named as packages are, as Poetry names its environments.
        assert_eq!(poetry_sanitized("My_Pkg..Core"), "my-pkg-core");
        assert_eq!(poetry_sanitized("Demo App"), "demo_app");
        // Pipenv: by its Pipfile.
        std::fs::remove_dir_all(&poetry).unwrap();
        std::fs::write(root.join("Pipfile"), "").unwrap();
        let workon = home.join("venvs");
        let pipenv = workon.join(format!(
            "Demo_Proj-{}",
            path_hash(real.join("Pipfile").to_string_lossy().as_bytes(), 6)
        ));
        python_at(&pipenv, "python");
        let workon_host = || host(vec![("WORKON_HOME", workon.clone().into_os_string())]);
        assert_eq!(
            found(&workon_host()),
            Some((python(&pipenv).unwrap(), Some(PythonEnvKind::Pipenv), true))
        );
        // pyenv: the version it pins, by name only.
        std::fs::remove_file(root.join("Pipfile")).unwrap();
        let pyenv = home.join("pyenv");
        python_at(&pyenv.join("versions/3.11.9"), "python");
        let pyenv_host = || host(vec![("PYENV_ROOT", pyenv.clone().into_os_string())]);
        assert_eq!(found(&pyenv_host()), None, "nothing pinned");
        std::fs::write(root.join(".python-version"), "3.11.9\n").unwrap();
        assert_eq!(
            found(&pyenv_host()),
            Some((
                python(&pyenv.join("versions/3.11.9")).unwrap(),
                Some(PythonEnvKind::Pyenv),
                true
            ))
        );
        std::fs::write(root.join(".python-version"), "../../active-env\n").unwrap();
        assert_eq!(found(&pyenv_host()), None, "a path is no version");
        // Conda's `base` with no environment of the project's own: used.
        std::fs::remove_file(root.join(".python-version")).unwrap();
        let base = host(vec![
            ("CONDA_PREFIX", conda.clone().into_os_string()),
            ("CONDA_DEFAULT_ENV", "base".into()),
        ]);
        assert_eq!(found(&base).map(|f| f.0), python(&conda));
    }

    /// Where the tools keep a project's Python beyond the root's own venv:
    /// `.python-version` pins by release (`3.12` the newest 3.12) in its
    /// folder or one above, through pyenv (and pyenv-win's layout) or uv;
    /// Hatch's environment; the conda environment `environment.yml` names;
    /// PDM's `.pdm-python`; `UV_PROJECT_ENVIRONMENT`; one venv a folder
    /// down in a monorepo (two is no guess); Poetry's `config.toml` path
    /// and the Windows places of Poetry and Pipenv.
    #[test]
    #[cfg(unix)]
    fn every_tools_environment_for_the_project_is_found() {
        let python_at = |dir: &Path| {
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            std::fs::write(dir.join("bin/python"), "").unwrap();
            dir.join("bin/python")
        };
        let home = scratch("tools-home");
        let parent = scratch("tools-parent");
        let root = parent.join("svc");
        std::fs::create_dir_all(&root).unwrap();
        let host = |vars: Vec<(&'static str, OsString)>| PythonHost {
            home: Some(home.clone()),
            path: None,
            vars,
            macos: false,
            windows: false,
        };
        let found =
            |host: &PythonHost| find_vetted_on(&root, host).map(|v| (v.interpreter, v.managed));
        // `.python-version` one folder up, pinning a release series.
        std::fs::write(parent.join(".python-version"), "# pinned\n3.12\n").unwrap();
        let pyenv = home.join("pyenv");
        python_at(&pyenv.join("versions/3.11.9"));
        python_at(&pyenv.join("versions/3.12.4"));
        let newest = python_at(&pyenv.join("versions/3.12.10"));
        python_at(&pyenv.join("versions/3.1.0"));
        let pyenv_host = || host(vec![("PYENV_ROOT", pyenv.clone().into_os_string())]);
        assert_eq!(
            found(&pyenv_host()),
            Some((newest, Some(PythonEnvKind::Pyenv)))
        );
        // pyenv-win keeps them a folder further down.
        std::fs::remove_dir_all(pyenv.join("versions")).unwrap();
        let win = python_at(&pyenv.join("pyenv-win/versions/3.12.1"));
        assert_eq!(
            found(&pyenv_host()),
            Some((win, Some(PythonEnvKind::Pyenv)))
        );
        // No pyenv: a Python uv installed.
        let uv = home.join("uvpy");
        let uv_python = python_at(&uv.join("cpython-3.12.7-linux-x86_64-gnu"));
        python_at(&uv.join("cpython-3.13.0-linux-x86_64-gnu"));
        assert_eq!(
            found(&host(vec![(
                "UV_PYTHON_INSTALL_DIR",
                uv.clone().into_os_string()
            )])),
            Some((uv_python, Some(PythonEnvKind::Uv)))
        );
        std::fs::remove_file(parent.join(".python-version")).unwrap();
        // Hatch: by the project's name, its default environment first.
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname = \"tea-shop\"\n",
        )
        .unwrap();
        let hatch = home.join("hatch");
        python_at(&hatch.join("env/virtual/tea-shop/AbCd1234/test"));
        let hatch_default = python_at(&hatch.join("env/virtual/tea-shop/AbCd1234/tea-shop"));
        python_at(&hatch.join("env/virtual/other/Zz/other"));
        assert_eq!(
            found(&host(vec![(
                "HATCH_DATA_DIR",
                hatch.clone().into_os_string()
            )])),
            Some((hatch_default, Some(PythonEnvKind::Hatch)))
        );
        // conda: the environment `environment.yml` names.
        std::fs::write(
            root.join("environment.yml"),
            "name: shop-ml\ndependencies:\n  - python\n",
        )
        .unwrap();
        let conda = home.join("miniconda3");
        python_at(&conda);
        let named = python_at(&conda.join("envs/shop-ml"));
        assert_eq!(
            found(&host(Vec::new())),
            Some((named, Some(PythonEnvKind::Conda)))
        );
        std::fs::remove_file(root.join("environment.yml")).unwrap();
        // PDM's interpreter for the project, kept outside it.
        let pdm_venv = home.join("pdm/venvs/tea-shop-x1");
        let pdm = python_at(&pdm_venv);
        std::fs::write(root.join(".pdm-python"), format!("{}\n", pdm.display())).unwrap();
        assert_eq!(
            found(&host(Vec::new())),
            Some((pdm.clone(), Some(PythonEnvKind::Pdm)))
        );
        let listed = python_candidates(&root, &host(Vec::new()));
        assert!(
            listed
                .iter()
                .any(|c| c.kind == PythonEnvKind::Pdm && c.name == "tea-shop-x1"),
            "{listed:?}"
        );
        // Not a Python by its name: not taken.
        std::fs::write(root.join(".pdm-python"), "/bin/sh\n").unwrap();
        assert_eq!(found(&host(Vec::new())), None);
        std::fs::remove_file(root.join(".pdm-python")).unwrap();
        // `UV_PROJECT_ENVIRONMENT` set by the user to one outside.
        let uv_env = home.join("uv-envs/shop");
        let uv_env_python = python_at(&uv_env);
        assert_eq!(
            found(&host(vec![(
                "UV_PROJECT_ENVIRONMENT",
                uv_env.clone().into_os_string()
            )])),
            Some((uv_env_python, None))
        );
        // A monorepo's one venv a folder down; two is no guess.
        let base = system_python("tools-base", BASE_BYTES);
        let backend = venv_bin(&root, "backend/.venv");
        std::os::unix::fs::symlink(base.join("python3.12"), backend.join("python")).unwrap();
        assert_eq!(
            found(&host(Vec::new())).map(|f| f.0),
            Some(backend.join("python"))
        );
        let worker = venv_bin(&root, "worker/venv");
        std::os::unix::fs::symlink(base.join("python3.12"), worker.join("python")).unwrap();
        assert_eq!(found(&host(Vec::new())), None);
        // Poetry: the path its config.toml names, `{cache-dir}` spelled out.
        let config = home.join("config/pypoetry");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.toml"),
            "cache-dir = \"/opt/poetry-cache\"\n[virtualenvs]\npath = \"{cache-dir}/envs\"\n",
        )
        .unwrap();
        let poetry_host = host(vec![(
            "XDG_CONFIG_HOME",
            home.join("config").into_os_string(),
        )]);
        assert_eq!(
            poetry_dir(&poetry_host),
            Some(PathBuf::from("/opt/poetry-cache/envs"))
        );
        // On Windows: Poetry under %LOCALAPPDATA%, Pipenv under ~/.virtualenvs.
        let windows = PythonHost {
            home: Some(home.clone()),
            path: None,
            vars: vec![
                ("LOCALAPPDATA", home.join("Local").into_os_string()),
                ("APPDATA", home.join("Roaming").into_os_string()),
            ],
            macos: false,
            windows: true,
        };
        assert_eq!(
            poetry_dir(&windows),
            Some(home.join("Local/pypoetry/Cache/virtualenvs"))
        );
        assert_eq!(pipenv_dir(&windows), Some(home.join(".virtualenvs")));
        assert!(version_pinned("3.12.4", "3.12") && !version_pinned("3.120.1", "3.12"));
    }

    /// The same python3 by two folders of the search path, one a link to
    /// the other (/bin and /usr/bin on a merged-/usr system), is one system
    /// interpreter, not two.
    #[cfg(unix)]
    #[test]
    fn one_system_python_by_two_folders_is_listed_once() {
        let home = scratch("merged-usr-home");
        let usr_bin = home.join("usr/bin");
        std::fs::create_dir_all(&usr_bin).unwrap();
        std::fs::write(usr_bin.join("python3"), "").unwrap();
        let bin = home.join("bin");
        std::os::unix::fs::symlink(&usr_bin, &bin).unwrap();
        let host = PythonHost {
            home: Some(home.clone()),
            path: Some(std::env::join_paths([&bin, &usr_bin]).unwrap()),
            vars: Vec::new(),
            macos: false,
            windows: false,
        };
        let system: Vec<String> = python_candidates(&home, &host)
            .into_iter()
            .filter(|c| {
                c.kind == PythonEnvKind::System && c.interpreter.starts_with(home.to_str().unwrap())
            })
            .map(|c| c.interpreter)
            .collect();
        assert_eq!(
            system,
            [bin.join("python3").to_string_lossy().into_owned()],
            "listed once, by the search path's first name for it"
        );
    }

    #[test]
    fn explicit_init_options_win_over_auto() {
        let auto = json!({ "python": { "pythonPath": "/auto", "analysis": { "level": "basic" } } });
        let mut merged = auto;
        deep_merge(
            &mut merged,
            json!({ "python": { "pythonPath": "/explicit" } }),
        );
        // Explicit overrides the conflicting key…
        assert_eq!(merged["python"]["pythonPath"], "/explicit");
        // …while non-conflicting auto keys survive.
        assert_eq!(merged["python"]["analysis"]["level"], "basic");
    }

    /// The user's Rust, Go and C/C++ choices become their servers' settings
    /// — only what they set, and only values the tools can take.
    #[test]
    fn build_choices_become_their_servers_settings() {
        let root = Path::new("/p");
        let rust = |r: RustChoices| {
            chosen(
                "rust",
                root,
                &LanguageChoices {
                    rust: Some(r),
                    ..Default::default()
                },
            )
        };
        assert_eq!(
            rust(RustChoices {
                features: vec!["serde".into(), "cli/color".into(), "bad feature".into()],
                no_default_features: true,
                target: Some("wasm32-unknown-unknown".into()),
                ..Default::default()
            }),
            Some(json!({ "cargo": {
                "features": ["serde", "cli/color"],
                "noDefaultFeatures": true,
                "target": "wasm32-unknown-unknown",
            }}))
        );
        assert_eq!(
            rust(RustChoices {
                all_features: true,
                features: vec!["ignored".into()],
                ..Default::default()
            }),
            Some(json!({ "cargo": { "features": "all" } }))
        );
        assert_eq!(rust(RustChoices::default()), None);

        let go = chosen(
            "go",
            root,
            &LanguageChoices {
                go: Some(GoChoices {
                    tags: vec!["integration".into(), "a,b".into(), " e2e ".into()],
                    goos: Some("windows".into()),
                    goarch: Some("ARM 64".into()),
                }),
                ..Default::default()
            },
        );
        assert_eq!(
            go,
            Some(json!({
                "buildFlags": ["-tags=integration,e2e"],
                "env": { "GOOS": "windows" },
            }))
        );

        let c = LanguageChoices {
            c: Some(CChoices {
                compile_commands: Some("build/debug".into()),
                flags: vec!["-DDEBUG".into(), " ".into(), "-Iinclude".into()],
            }),
            ..Default::default()
        };
        let expected = json!({
            "compilationDatabasePath": root.join("build/debug"),
            "fallbackFlags": ["-DDEBUG", "-Iinclude"],
        });
        assert_eq!(chosen("c", root, &c), Some(expected.clone()));
        assert_eq!(chosen("cpp", root, &c), Some(expected));
        // A choice for one language says nothing to another's server.
        assert_eq!(chosen("rust", root, &c), None);
    }

    /// The chosen build tags join the flags the project's `lsp.toml` gives
    /// gopls, in place of its own `-tags`, and its other settings stand.
    #[test]
    fn chosen_go_tags_join_the_projects_other_build_flags() {
        let server = crate::lsp::config::ProjectLspConfig::default()
            .resolve("go")
            .unwrap();
        let choices = LanguageChoices {
            go: Some(GoChoices {
                tags: vec!["e2e".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let explicit = json!({
            "buildFlags": ["-mod=vendor", "-tags=old", "--tags", "x"],
            "env": { "GOFLAGS": "-v" },
            "staticcheck": true,
        });
        let merged = merge(
            "go",
            &server,
            Path::new("/store/gopls"),
            Path::new("/p"),
            Some(explicit),
            &choices,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            merged["buildFlags"],
            json!(["-mod=vendor", "x", "-tags=e2e"])
        );
        assert_eq!(merged["env"], json!({ "GOFLAGS": "-v" }));
        assert_eq!(merged["staticcheck"], true);
    }

    /// A program the user chose runs with no settings pinned for clew's own
    /// binary: the pin would look beside clew's install, not theirs.
    #[test]
    fn a_chosen_server_program_gets_no_pins() {
        let mut server = crate::lsp::config::ProjectLspConfig::default()
            .resolve("typescript")
            .unwrap();
        server.chosen = Some("/opt/bin/typescript-language-server".into());
        assert_eq!(
            pinned(&server, Path::new("/opt/bin/typescript-language-server")),
            Ok(None)
        );
    }

    /// A chosen TypeScript reaches the TypeScript and JavaScript servers as
    /// the `tsserver.js` it names — given as the file, its `lib` folder or
    /// the package — over clew's own; one that names none leaves clew's in
    /// place rather than letting the server fall back to the project's.
    /// A chosen `zig` reaches zls.
    #[test]
    #[cfg(unix)]
    fn a_chosen_typescript_and_zig_reach_their_servers() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("chosen-ts-zig");
        let package = dir.join("ts");
        std::fs::create_dir_all(package.join("lib")).unwrap();
        let tsserver = package.join("lib/tsserver.js");
        std::fs::write(&tsserver, "// tsserver").unwrap();
        let zig = dir.join("bin/zig");
        std::fs::create_dir_all(zig.parent().unwrap()).unwrap();
        std::fs::write(&zig, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&zig, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ts = |path: &Path| LanguageChoices {
            typescript: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        };
        for given in [&tsserver, &package.join("lib"), &package] {
            for language in ["typescript", "tsx", "javascript"] {
                assert_eq!(
                    chosen(language, &dir, &ts(given)),
                    Some(json!({ "tsserver": { "path": tsserver.to_string_lossy() } })),
                    "{given:?} for {language}"
                );
            }
        }
        assert_eq!(chosen("typescript", &dir, &ts(&dir.join("gone"))), None);

        // Over clew's own pin, in the merge.
        let server = crate::lsp::config::ProjectLspConfig::default()
            .resolve("typescript")
            .unwrap();
        let store = dir.join("store/node_modules");
        std::fs::create_dir_all(store.join(".bin")).unwrap();
        std::fs::create_dir_all(store.join("typescript/lib")).unwrap();
        std::fs::write(store.join("typescript/lib/tsserver.js"), "// clew's").unwrap();
        let exe = store.join(".bin/typescript-language-server");
        let merged = merge("typescript", &server, &exe, &dir, None, &ts(&package))
            .unwrap()
            .unwrap();
        assert_eq!(merged["tsserver"]["path"], tsserver.to_str().unwrap());
        let kept = merge(
            "typescript",
            &server,
            &exe,
            &dir,
            None,
            &ts(&dir.join("gone")),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            kept["tsserver"]["path"],
            store.join("typescript/lib/tsserver.js").to_str().unwrap()
        );

        let zig_choice = LanguageChoices {
            zig: Some(zig.to_string_lossy().into_owned()),
            ..Default::default()
        };
        assert_eq!(
            chosen("zig", &dir, &zig_choice),
            Some(json!({ "zig_exe_path": zig.to_string_lossy() }))
        );
    }

    /// Java's JDK, PHP's version and C#'s solution reach their servers'
    /// settings — a JDK by its Java SE version as jdtls names runtimes, a
    /// PHP version in full — and a choice that names nothing usable reaches
    /// nothing.
    #[test]
    fn a_chosen_jdk_php_version_and_solution_reach_their_servers() {
        let dir = scratch("chosen-java-php-cs");
        let jdk = dir.join("jdks/temurin-21");
        std::fs::create_dir_all(jdk.join("bin")).unwrap();
        std::fs::write(jdk.join("bin/java"), "").unwrap();
        std::fs::write(jdk.join("release"), "JAVA_VERSION=\"21.0.1\"\n").unwrap();
        let jdk8 = dir.join("jdks/jdk8");
        std::fs::create_dir_all(jdk8.join("bin")).unwrap();
        std::fs::write(jdk8.join("bin/java"), "").unwrap();
        std::fs::write(jdk8.join("release"), "JAVA_VERSION=\"1.8.0_392\"\n").unwrap();
        let choose = |c: LanguageChoices| c;
        let java = |home: &Path| {
            chosen(
                "java",
                &dir,
                &choose(LanguageChoices {
                    java: Some(home.to_string_lossy().into_owned()),
                    ..Default::default()
                }),
            )
        };
        let settings = java(&jdk).unwrap();
        assert_eq!(settings["settings"]["java"]["home"], jdk.to_str().unwrap());
        assert_eq!(
            settings["settings"]["java"]["configuration"]["runtimes"][0]["name"],
            "JavaSE-21"
        );
        assert_eq!(
            java(&jdk8).unwrap()["settings"]["java"]["configuration"]["runtimes"][0]["name"],
            "JavaSE-1.8"
        );
        assert_eq!(java(&dir.join("jdks")), None, "no bin/java: no JDK");

        let php = |v: &str| {
            chosen(
                "php",
                &dir,
                &LanguageChoices {
                    php: Some(v.into()),
                    ..Default::default()
                },
            )
        };
        assert_eq!(
            php("8.2"),
            Some(json!({ "intelephense": { "environment": { "phpVersion": "8.2.0" } } }))
        );
        assert_eq!(php("8.x"), None);
        assert_eq!(php("8.2000"), None, "no part is that long");
        assert_eq!(
            php("8.2.1"),
            Some(json!({ "intelephense": { "environment": { "phpVersion": "8.2.1" } } }))
        );

        std::fs::write(dir.join("Shop.sln"), "").unwrap();
        let cs = |s: &str| {
            chosen(
                "csharp",
                &dir,
                &LanguageChoices {
                    csharp: Some(s.into()),
                    ..Default::default()
                },
            )
        };
        assert_eq!(
            cs("Shop.sln"),
            Some(json!({ "csharp": { "solution": dir.join("Shop.sln").to_string_lossy() } }))
        );
        assert_eq!(cs("Gone.sln"), None);
        std::fs::write(dir.join("Shop.csproj"), "").unwrap();
        assert_eq!(cs("Shop.csproj"), None, "a project is not a solution");
    }
}
