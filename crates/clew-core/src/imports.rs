//! Raw-import extraction: the import specifiers a file writes down, read off
//! its syntax tree. Pure per file and cacheable — this is the *extraction*
//! half of the import graph; *resolution* (mapping specifiers onto the
//! project's file set) stays in the client, since it is a pure computation
//! over the file list.
//!
//! Lives in clew-core so both the client's local indexer and the server's
//! project-symbol snapshot extract identically — a remote client receives
//! these over the protocol instead of reading remote-pathed files off its
//! own disk.

use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, Tree};

use crate::highlight::{Lang, QueryCache, QueryError};

/// One import as written in a file, before resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawImport {
    /// The specifier text, normalized per language: a Rust `use` path
    /// (`crate::lsp::client` — a grouped `use a::{b, c}` yields one entry per
    /// path it names; a glob keeps its `::*` and a re-export is prefixed
    /// `pub `, see [`crate::rustscope::RustUse`]), a Python dotted/relative
    /// module (`os.path`, `.sibling`; `from . import views` names `.:views`,
    /// see [`PY_NAME_SEP`]), a JS/TS specifier (`./util`, `react`, including
    /// `require()` and dynamic `import()` of a literal), a Go import path, a
    /// Lean or Haskell module (`Mathlib.Data.Nat.Basic`, `Data.Map`), a Java
    /// import (`a.b.C`, `a.b.*`, a static one's member `a.b.C.m`), a C#
    /// `using`'s namespace or type (`A.B`, its alias and `static` dropped), a
    /// PHP name (`A\B\C`, one per entry of a group) or `require`d path
    /// (`x.php`; `__DIR__ . '/x.php'` is `__DIR__/x.php`), a Ruby `require`
    /// (`x`; a `require_relative` is the relative path `./x`), an Erlang
    /// module (`-behaviour`, `-import`) or header (`x.hrl`; an `-include_lib`
    /// is prefixed [`ERLANG_LIB`]), or a Zig `@import` (`x.zig`, `std`).
    pub module: String,
    /// 1-based line of the statement (for navigation).
    pub line: usize,
    /// A declaration rather than an import. Rust `mod x;` — `module` is a
    /// submodule name to resolve to a sibling file, not a scoped path. Java
    /// `package a.b;` and C# `namespace A.B` — `module` is the package or
    /// namespace the file declares itself part of (a nested C# namespace by
    /// its full name): no edge of its own, but what the importer's own Java
    /// imports and other files' C# `using`s resolve against.
    pub is_mod_decl: bool,
}

/// Extract the raw imports from one file's `source`. Returns an empty list for
/// languages without an import model or when parsing fails. Blocking; run off
/// the UI thread.
pub fn imports_of(source: &str, lang: &str) -> Vec<RawImport> {
    let Some((lang, source)) = crate::highlight::read_as(lang, source) else {
        return Vec::new();
    };
    let source = &*source;
    // Fetch (or compile) the query first, so a language without an import
    // model is not parsed at all.
    if !is_web_page_or_style(lang) && import_query(lang).is_err() {
        return Vec::new();
    }
    let Some(tree) = crate::highlight::parse(source, lang) else {
        return Vec::new();
    };
    imports_in(&tree, source, lang)
}

/// The imports of an already-parsed file (`tree` is `source` parsed with
/// `lang`'s grammar), for callers that derive several facts from one parse.
pub fn imports_in(tree: &Tree, source: &str, lang: Lang) -> Vec<RawImport> {
    let root = tree.root_node();
    match lang {
        Lang::Html => return html_imports(root, source),
        Lang::Css | Lang::Scss | Lang::Less => return style_imports(root, source),
        _ => {}
    }
    let Ok(query) = import_query(lang) else {
        return Vec::new();
    };
    match lang {
        Lang::Rust => rust_imports(root, source, query),
        Lang::Python => python_imports(root, source, query),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => js_imports(root, source, query),
        Lang::Go | Lang::Dart => path_imports(root, source, query),
        Lang::Lean => lean_imports(root, source, query),
        Lang::Java => java_imports(root, source, query),
        Lang::CSharp => csharp_imports(root, source, query),
        Lang::Php => php_imports(root, source, query),
        Lang::Ruby => ruby_imports(root, source, query),
        Lang::Erlang => erlang_imports(root, source, query),
        Lang::Haskell => haskell_imports(root, source, query),
        Lang::Zig => zig_imports(root, source, query),
        Lang::Kotlin => kotlin_imports(root, source, query),
        Lang::Scala => scala_imports(root, source, query),
        Lang::Elixir => elixir_imports(root, source, query),
        Lang::C | Lang::Cpp => c_includes(root, source, query),
        Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
        | Lang::Scss
        | Lang::Less
        | Lang::Vue
        | Lang::Svelte
        | Lang::Astro
        | Lang::Sql => Vec::new(),
    }
}

/// The import query for `lang`, compiled once per process; `Unsupported` for
/// a language without an import model.
pub fn import_query(lang: Lang) -> Result<&'static Query, QueryError> {
    static QUERIES: QueryCache = QueryCache::new("import");
    QUERIES.get(lang, || match lang {
        Lang::Rust => Some(RUST_QUERY),
        Lang::Python => Some(PYTHON_QUERY),
        Lang::JavaScript => Some(JS_QUERY),
        Lang::TypeScript | Lang::Tsx => Some(TS_QUERY),
        Lang::Go => Some(GO_QUERY),
        Lang::Dart => Some(DART_QUERY),
        Lang::Lean => Some(LEAN_QUERY),
        Lang::Java => Some(JAVA_QUERY),
        Lang::CSharp => Some(CSHARP_QUERY),
        Lang::Php => Some(PHP_QUERY),
        Lang::Ruby => Some(RUBY_QUERY),
        Lang::Erlang => Some(ERLANG_QUERY),
        Lang::Haskell => Some(HASKELL_QUERY),
        Lang::Zig => Some(ZIG_QUERY),
        Lang::Kotlin => Some(KOTLIN_QUERY),
        Lang::Scala => Some(SCALA_QUERY),
        Lang::Elixir => Some(ELIXIR_QUERY),
        Lang::C | Lang::Cpp => Some(C_QUERY),
        Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
        | Lang::Scss
        | Lang::Less
        | Lang::Vue
        | Lang::Svelte
        | Lang::Astro
        | Lang::Sql => None,
    })
}

/// Byte cap for a project metadata file (`go.mod`, `pubspec.yaml`,
/// `composer.json`, a `.cabal` file, `package.yaml`). Each ships with the
/// repository, so its size and type are attacker-chosen, and each is read
/// automatically on every project open — a `go.mod -> /dev/zero` would
/// otherwise read until the process died. Real ones are a few KB.
const MAX_METADATA_BYTES: u64 = 1024 * 1024;

/// The `module` line from a project root's `go.mod`, if any. Shared so the
/// server can ship it to a remote client (whose resolver must not read the
/// remote-pathed file off the local disk).
///
/// Read through the bounded, plain-file-only reader: this file is part of the
/// repository (see [`MAX_METADATA_BYTES`]). A file that is a symlink, a FIFO,
/// a device, or over the cap simply reads as "no module declared", which
/// degrades import resolution instead of hanging the open.
pub fn read_go_module(root: &Path) -> Option<String> {
    let text = crate::statefile::read_capped(&root.join("go.mod"), MAX_METADATA_BYTES)?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("module ") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// The package `name:` from `pubspec.yaml` — a top-level (unindented) key, so a
/// nested `name:` under `dependencies:` isn't mistaken for it. Bounded and
/// plain-file-only for the same reason as [`read_go_module`].
pub fn read_dart_package(root: &Path) -> Option<String> {
    let text = crate::statefile::read_capped(&root.join("pubspec.yaml"), MAX_METADATA_BYTES)?;
    for line in text.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        if let Some(rest) = line.strip_prefix("name:") {
            let name = rest.trim().trim_matches(['"', '\'']);
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// The PSR-4 autoload map of the `composer.json` at `path`, `autoload` and
/// `autoload-dev` alike: each namespace prefix (`App\`, without a leading
/// backslash; empty for a fallback directory) with the directories its
/// classes live under, relative to the file's own directory, in the order
/// they are searched. The prefixes of a section come sorted, not in file
/// order: which one applies is the resolver's to decide, the longest first.
/// Bounded and plain-file-only for the same reason as [`read_go_module`]; a
/// file that is not there, not read or not JSON maps nothing.
pub fn read_composer_psr4(path: &Path) -> Vec<(String, Vec<String>)> {
    let Some(text) = crate::statefile::read_capped(path, MAX_METADATA_BYTES) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for section in ["autoload", "autoload-dev"] {
        let Some(map) = json
            .get(section)
            .and_then(|s| s.get("psr-4"))
            .and_then(|m| m.as_object())
        else {
            continue;
        };
        for (prefix, dirs) in map {
            // One directory, or several searched in order.
            let dirs: Vec<String> = match dirs {
                serde_json::Value::String(dir) => vec![dir.clone()],
                serde_json::Value::Array(dirs) => dirs
                    .iter()
                    .filter_map(|d| d.as_str().map(str::to_string))
                    .collect(),
                _ => continue,
            };
            out.push((prefix.trim_start_matches('\\').to_string(), dirs));
        }
    }
    out
}

/// Most manifests of one kind (`composer.json`, Haskell package
/// descriptions) [`source_manifests`] reads: a monorepo has one per package,
/// a pathological tree thousands. The shallowest are kept.
pub const MAX_MANIFESTS: usize = 64;

/// A Haskell package description's file name: a `.cabal` file or hpack's
/// `package.yaml`.
pub fn is_haskell_manifest(name: &str) -> bool {
    name == "package.yaml" || (name.ends_with(".cabal") && name.len() > ".cabal".len())
}

/// Whether a project file (by rel) bears on [`source_manifests`]: a manifest
/// it reads, or a source file of a language whose manifests it reads only
/// when the project has one.
pub fn bears_on_manifests(rel: &str) -> bool {
    let name = rel.rsplit(['/', '\\']).next().unwrap_or(rel);
    name == "composer.json"
        || name == "package.json"
        || name == "Cargo.toml"
        || name == "pyproject.toml"
        || name == "build.zig"
        || is_lakefile(name)
        || is_haskell_manifest(name)
        || [".php", ".hs", ".lhs", ".lean"]
            .iter()
            .any(|ext| name.ends_with(ext))
}

/// What the project's package manifests, among its files `rels`, say of
/// where its sources are: each `composer.json`'s PSR-4 map when the project
/// has PHP, each Haskell package description's source directories when it
/// has Haskell — only then, as `package.yaml` is pnpm's file too; each Cargo
/// package's targets, each `pyproject.toml`'s Python source roots, and
/// the modules each `build.zig` names ([`read_zig_modules`]). The
/// shallowest [`MAX_MANIFESTS`] of each kind ([`MAX_CARGO_MANIFESTS`] of
/// Cargo's: a workspace has one per crate), read bounded and plain-file-only,
/// where the files live.
pub fn source_manifests<S: AsRef<str>>(root: &Path, rels: &[S]) -> clew_protocol::SourceManifests {
    let rels: Vec<&str> = rels.iter().map(AsRef::as_ref).collect();
    let has = |exts: &[&str]| rels.iter().any(|r| exts.iter().any(|ext| r.ends_with(ext)));
    let picked_up_to = |is_manifest: &dyn Fn(&str) -> bool, most: usize| {
        let mut found: Vec<&str> = rels
            .iter()
            .copied()
            .filter(|r| is_manifest(r.rsplit(['/', '\\']).next().unwrap_or(r)))
            .collect();
        found.sort_by_key(|r| (r.split(['/', '\\']).count(), *r));
        found.truncate(most);
        found
    };
    let picked = |is_manifest: &dyn Fn(&str) -> bool| picked_up_to(is_manifest, MAX_MANIFESTS);
    let mut out = clew_protocol::SourceManifests::default();
    if has(&[".php"]) {
        for rel in picked(&|name| name == "composer.json") {
            let map = read_composer_psr4(&root.join(rel));
            if !map.is_empty() {
                out.psr4.push((rel.to_string(), map));
            }
        }
    }
    if has(&[".hs", ".lhs"]) {
        for rel in picked(&is_haskell_manifest) {
            let path = root.join(rel);
            let dirs = match rel.ends_with("package.yaml") {
                true => read_hpack_source_dirs(&path),
                false => read_cabal_source_dirs(&path),
            };
            out.haskell.push((rel.to_string(), dirs));
        }
    }
    for rel in picked_up_to(&|name| name == "Cargo.toml", MAX_CARGO_MANIFESTS) {
        if let Some(package) = read_cargo_package(&root.join(rel)) {
            out.cargo.push((rel.to_string(), package));
        }
    }
    for rel in picked(&|name| name == "pyproject.toml") {
        let roots = read_python_source_roots(&root.join(rel));
        if !roots.is_empty() {
            out.python.push((rel.to_string(), roots));
        }
    }
    // A build.zig is itself Zig: where there is one, there is Zig. Each is
    // listed, naming modules or not: its files see its names alone.
    for rel in picked(&|name| name == "build.zig") {
        out.zig
            .push((rel.to_string(), read_zig_modules(&root.join(rel))));
    }
    if has(&[".lean"]) {
        for rel in picked(&is_lakefile) {
            out.lean
                .push((rel.to_string(), read_lake_src_dirs(&root.join(rel))));
        }
    }
    // Read whatever the project's languages: only a JavaScript project has
    // them, and a project's first script is no reason to read them all
    // again at every save of one.
    for rel in picked_up_to(&|name| name == "package.json", MAX_CARGO_MANIFESTS) {
        if let Some(package) = read_js_package(&root.join(rel)) {
            out.js.push((rel.to_string(), package));
        }
    }
    out
}

/// What the `package.json` at `path` says of the package
/// ([`clew_protocol::JsPackage`]); `None` for one with no name, which no
/// import can name.
pub fn read_js_package(path: &Path) -> Option<clew_protocol::JsPackage> {
    let text = crate::statefile::read_capped(path, MAX_METADATA_BYTES)?;
    js_package(&text)
}

fn js_package(text: &str) -> Option<clew_protocol::JsPackage> {
    use serde_json::Value;
    let json: Value = serde_json::from_str(text).ok()?;
    let name = json.get("name")?.as_str()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    // Every file a condition object (`{"types": …, "import": …}`) or an
    // array of fallbacks names, in its order.
    fn targets(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(s) if plain_relative(s.trim_start_matches("./")) => {
                out.push(s.clone());
            }
            Value::Array(items) => items.iter().for_each(|v| targets(v, out)),
            Value::Object(map) => map.values().for_each(|v| targets(v, out)),
            _ => {}
        }
    }
    let mut exports: Vec<(String, Vec<String>)> = Vec::new();
    match json.get("exports") {
        Some(Value::Object(map)) if map.keys().any(|k| k.starts_with('.')) => {
            for (subpath, value) in map {
                let mut files = Vec::new();
                targets(value, &mut files);
                exports.push((subpath.clone(), files));
            }
        }
        Some(value @ (Value::String(_) | Value::Array(_) | Value::Object(_))) => {
            let mut files = Vec::new();
            targets(value, &mut files);
            exports.push((".".to_string(), files));
        }
        _ => {}
    }
    // The fields older tools read, after what `exports` says of `.`.
    let fields: Vec<String> = ["source", "module", "main", "types", "typings", "svelte"]
        .iter()
        .filter_map(|f| json.get(*f)?.as_str())
        .filter(|s| plain_relative(s.trim_start_matches("./")))
        .map(str::to_string)
        .collect();
    match exports.iter_mut().find(|(subpath, _)| subpath == ".") {
        Some((_, files)) => files.extend(fields),
        None if !fields.is_empty() => exports.insert(0, (".".to_string(), fields)),
        None => {}
    }
    // Its sources before what a build makes of them: a `.ts` file before a
    // `.js`, and anything before `dist/`.
    let built = |f: &str| {
        let f = f.trim_start_matches("./");
        ["dist/", "build/", "out/", "lib/", "esm/", "cjs/"]
            .iter()
            .any(|d| f.starts_with(d))
    };
    let source = |f: &str| {
        [".ts", ".tsx", ".mts", ".cts", ".vue", ".svelte"]
            .iter()
            .any(|e| f.ends_with(e))
            && !f.ends_with(".d.ts")
    };
    for (_, files) in &mut exports {
        files.dedup();
        files.sort_by_key(|f| (built(f), !source(f)));
    }
    Some(clew_protocol::JsPackage { name, exports })
}

/// Whether a file name is a Lake package's lakefile.
pub fn is_lakefile(name: &str) -> bool {
    matches!(name, "lakefile.lean" | "lakefile.toml")
}

/// The source directories the lakefile at `path` names for its libraries
/// and executables (`srcDir := "lib"`, `srcDir = "lib"`), relative to its
/// own folder; none for one that names none (its folder is the root). Read
/// as text, bounded: a `lakefile.lean` is a Lean program, not run.
pub fn read_lake_src_dirs(path: &Path) -> Vec<String> {
    let Some(text) = crate::statefile::read_capped(path, 256 * 1024) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for (at, _) in text.match_indices("srcDir") {
        let rest = text[at + "srcDir".len()..].trim_start();
        let rest = rest
            .strip_prefix(":=")
            .or_else(|| rest.strip_prefix('='))
            .map(str::trim_start);
        let Some(literal) = rest.and_then(|r| r.strip_prefix('"')) else {
            continue;
        };
        let Some(close) = literal.find('"') else {
            continue;
        };
        let dir = literal[..close].trim_matches('/').trim_start_matches("./");
        if !dir.is_empty() && !dir.contains("..") && !dirs.iter().any(|d| d == dir) {
            dirs.push(dir.to_string());
        }
    }
    dirs
}

/// The modules the `build.zig` at `path` names: each name code
/// `@import`s a module by (`.imports = &.{ .{ .name = "greet", .module =
/// greet } }`, `addImport("greet", greet)`, `b.addModule("greet", …)`,
/// `addAnonymousImport`, Zig 0.11's `.dependencies` and `addModule`) with
/// the module's root source file where the build gives one of the project's
/// (`b.path("src/greet.zig")`, `.{ .path = "src/greet.zig" }`), relative to
/// the build.zig's folder. A dependency's module (`dep.module("zap")`) or
/// one the build generates (`addOptions`) has no file here: not listed.
/// Read as text, bounded: a build.zig is a Zig program, not run.
pub fn read_zig_modules(path: &Path) -> Vec<(String, String)> {
    crate::statefile::read_capped(path, 256 * 1024)
        .map(|text| zig_modules(&text))
        .unwrap_or_default()
}

fn zig_modules(text: &str) -> Vec<(String, String)> {
    let text = zig_without_comments(text);
    let text = text.as_str();
    // The modules bound to a name in the build (`const greet = b.createModule(…)`).
    let mut vars: std::collections::HashMap<String, String> = Default::default();
    let mut named: Vec<(String, String)> = Vec::new();
    for keyword in ["const ", "var "] {
        for (at, _) in text.match_indices(keyword) {
            let before = text[..at].chars().next_back();
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
                continue;
            }
            let rest = text[at + keyword.len()..].trim_start();
            let end = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let (var, after) = rest.split_at(end);
            // `const x: T = value;`: the value, to its `;`.
            let Some(eq) = after
                .find(['=', ';'])
                .filter(|&i| after.as_bytes()[i] == b'=')
            else {
                continue;
            };
            let value = after[eq + 1..].trim_start();
            let value = zig_statement(value);
            if let Some(file) = zig_module_file(value, &vars) {
                vars.insert(var.to_string(), file);
            }
        }
    }
    // Names given by a call: `addImport("greet", greet)`, `b.addModule("greet", .{ … })`.
    for method in [
        "addImport",
        "addModule",
        "addAnonymousImport",
        "addAnonymousModule",
    ] {
        let call = format!(".{method}(");
        for (at, _) in text.match_indices(&call) {
            let Some(args) = zig_balanced(text, at + call.len() - 1) else {
                continue;
            };
            let args = zig_split(args, b',');
            let (Some(name), Some(module)) = (args.first(), args.get(1)) else {
                continue;
            };
            let Some(name) = zig_string(name) else {
                continue;
            };
            let module = module.trim();
            let file = match zig_struct_body(module) {
                Some(body) => zig_root_file(body),
                None => zig_module_file(module, &vars),
            };
            if let Some(file) = file {
                named.push((name, file));
            }
        }
    }
    // Names given in a struct: `.{ .name = "greet", .module = greet }`.
    for (at, _) in text.match_indices(".{") {
        let Some(body) = zig_balanced(text, at + 1) else {
            continue;
        };
        let (Some(name), Some(module)) = (zig_field(body, "name"), zig_field(body, "module"))
        else {
            continue;
        };
        if let (Some(name), Some(file)) = (zig_string(name), zig_module_file(module, &vars)) {
            named.push((name, file));
        }
    }
    let mut seen = std::collections::HashSet::new();
    named.retain(|(name, _)| seen.insert(name.clone()));
    named.sort();
    named
}

/// The root file of the module `expr` is: a name bound to one, or a
/// `createModule`/`addModule` call's.
fn zig_module_file(expr: &str, vars: &std::collections::HashMap<String, String>) -> Option<String> {
    let expr = expr.trim().trim_start_matches('&');
    if expr.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return vars.get(expr).cloned();
    }
    for method in [".createModule(", ".addModule("] {
        let Some(at) = expr.find(method) else {
            continue;
        };
        // The call itself, not one somewhere in an argument.
        if !expr[..at]
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        {
            continue;
        }
        let args = zig_balanced(expr, at + method.len() - 1)?;
        let options = zig_split(args, b',').into_iter().last()?;
        return zig_root_file(zig_struct_body(options)?);
    }
    None
}

/// A module's options' root source file, if one of the project's:
/// `.root_source_file = b.path("src/x.zig")` (Zig 0.11's `.source_file`, a
/// `.{ .path = "…" }`).
fn zig_root_file(options: &str) -> Option<String> {
    let value =
        zig_field(options, "root_source_file").or_else(|| zig_field(options, "source_file"))?;
    let value = value.trim();
    let path = if let Some(body) = zig_struct_body(value) {
        zig_string(zig_field(body, "path")?)?
    } else {
        let open = value.find(".path(")? + ".path(".len() - 1;
        let prefix = &value[..open + 1 - ".path(".len()];
        if !prefix
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        {
            return None;
        }
        zig_string(zig_balanced(value, open)?)?
    };
    let path = path.trim_start_matches("./");
    (!path.is_empty() && !path.starts_with('/') && !path.split('/').any(|p| p == ".."))
        .then(|| path.to_string())
}

/// A struct literal's body (`.{ … }`, without the braces).
fn zig_struct_body(expr: &str) -> Option<&str> {
    let expr = expr.trim();
    let rest = expr.strip_prefix(".{")?;
    let body = zig_balanced(expr, 1)?;
    (rest.len() == body.len() + 1).then_some(body)
}

/// The value a struct literal's body gives `field` (`.field = value`), at
/// its own level.
fn zig_field<'a>(body: &'a str, field: &str) -> Option<&'a str> {
    zig_split(body, b',').into_iter().find_map(|part| {
        let rest = part.trim().strip_prefix('.')?.strip_prefix(field)?;
        rest.trim_start().strip_prefix('=').map(str::trim)
    })
}

/// A string literal's text (`"src/x.zig"`), when `expr` is one alone.
fn zig_string(expr: &str) -> Option<String> {
    let inner = expr.trim().strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('"') && !inner.contains('\\')).then(|| inner.to_string())
}

/// What the bracket at `open` (`(`, `{` or `[`) holds, up to its match;
/// strings skipped.
fn zig_balanced(text: &str, open: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &c) in bytes.iter().enumerate().skip(open) {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'(' | b'{' | b'[' => depth += 1,
            b')' | b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return text.get(open + 1..i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `text` up to its statement's end: the first `;` outside brackets and
/// strings, or the bracket closing around it.
fn zig_statement(text: &str) -> &str {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &c) in text.as_bytes().iter().enumerate() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'(' | b'{' | b'[' => depth += 1,
            b')' | b'}' | b']' if depth == 0 => return &text[..i],
            b')' | b'}' | b']' => depth -= 1,
            b';' if depth == 0 => return &text[..i],
            _ => {}
        }
    }
    text
}

/// `text` split at each `sep` outside brackets and strings.
fn zig_split(text: &str, sep: u8) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0;
    for (i, &c) in text.as_bytes().iter().enumerate() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'(' | b'{' | b'[' => depth += 1,
            b')' | b'}' | b']' => depth -= 1,
            c if c == sep && depth == 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < text.len() {
        parts.push(&text[start..]);
    }
    parts.retain(|p| !p.trim().is_empty());
    parts
}

/// Zig source without its `//` comments, nor its multiline string lines
/// (`\\…`), nor character literals (`'"'`), so brackets and quotes count.
fn zig_without_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if line.trim_start().starts_with("\\\\") {
            out.push('\n');
            continue;
        }
        let bytes = line.as_bytes();
        let mut in_string = false;
        let mut escaped = false;
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if in_string {
                match (escaped, c) {
                    (true, _) => escaped = false,
                    (false, b'\\') => escaped = true,
                    (false, b'"') => in_string = false,
                    _ => {}
                }
                out.push(c as char);
                i += 1;
                continue;
            }
            match c {
                b'/' if bytes.get(i + 1) == Some(&b'/') => break,
                b'"' => in_string = true,
                b'\'' => {
                    // A character literal: skipped whole.
                    let close = line[i + 1..]
                        .char_indices()
                        .skip(1)
                        .find(|(_, ch)| *ch == '\'')
                        .map(|(at, _)| i + 1 + at + 1);
                    if let Some(close) = close {
                        out.push_str("' '");
                        i = close;
                        continue;
                    }
                }
                _ => {}
            }
            if c.is_ascii() {
                out.push(c as char);
                i += 1;
            } else {
                let ch = line[i..].chars().next().unwrap_or(' ');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
        out.push('\n');
    }
    out
}

/// Most `Cargo.toml`s [`source_manifests`] reads: more than of any other
/// kind, as a workspace has one per crate and a large one hundreds.
pub const MAX_CARGO_MANIFESTS: usize = 512;

/// What the Cargo manifest at `path` says of its package's targets; `None`
/// for one with no `[package]` (a workspace's own) or none readable. Bounded
/// and plain-file-only for the same reason as [`read_go_module`].
pub fn read_cargo_package(path: &Path) -> Option<clew_protocol::CargoTargets> {
    let text = crate::statefile::read_capped(path, MAX_METADATA_BYTES)?;
    cargo_package(&text)
}

fn cargo_package(text: &str) -> Option<clew_protocol::CargoTargets> {
    let manifest: toml::Table = text.parse().ok()?;
    let name = manifest.get("package")?.get("name")?.as_str()?;
    let lib = manifest.get("lib").and_then(toml::Value::as_table);
    let lib_str = |key: &str| lib.and_then(|l| l.get(key)).and_then(toml::Value::as_str);
    let target_paths = ["bin", "example", "test", "bench"]
        .iter()
        .filter_map(|kind| manifest.get(*kind)?.as_array())
        .flatten()
        .filter_map(|target| target.get("path")?.as_str())
        .filter(|path| plain_relative(path))
        .map(str::to_string)
        .collect();
    Some(clew_protocol::CargoTargets {
        lib_name: lib_str("name").unwrap_or(name).replace('-', "_"),
        lib_path: lib_str("path")
            .filter(|path| plain_relative(path))
            .unwrap_or("src/lib.rs")
            .to_string(),
        target_paths,
    })
}

/// The Python source roots the `pyproject.toml` at `path` names, relative to
/// its directory. Bounded and plain-file-only for the same reason as
/// [`read_go_module`].
pub fn read_python_source_roots(path: &Path) -> Vec<String> {
    crate::statefile::read_capped(path, MAX_METADATA_BYTES)
        .map(|text| python_source_roots(&text))
        .unwrap_or_default()
}

/// Where a `pyproject.toml` says its Python lives, as each tool writes it:
/// setuptools' `packages.find.where` and `package-dir` root, Poetry's
/// packages' `from`, Hatch's wheel packages' folders, PDM's `package-dir`,
/// maturin's `python-source`, pytest's `pythonpath`, Pyright's (and
/// basedpyright's) `extraPaths`, and mypy's `mypy_path`. In the order
/// written, each once; `.` (the file's own folder) is left out, as the
/// project's root is looked in anyway.
fn python_source_roots(text: &str) -> Vec<String> {
    let Ok(manifest) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let at = |path: &[&str]| -> Option<&toml::Value> {
        let mut value = manifest.get("tool")?;
        for key in path {
            value = value.get(*key)?;
        }
        Some(value)
    };
    let strings = |value: Option<&toml::Value>| -> Vec<String> {
        match value {
            Some(toml::Value::String(s)) => vec![s.clone()],
            Some(toml::Value::Array(items)) => items
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    };
    let mut roots: Vec<String> = Vec::new();
    roots.extend(strings(at(&["setuptools", "packages", "find", "where"])));
    roots.extend(strings(at(&["setuptools", "package-dir", ""])));
    if let Some(packages) = at(&["poetry", "packages"]).and_then(toml::Value::as_array) {
        roots.extend(
            packages
                .iter()
                .filter_map(|p| p.get("from")?.as_str())
                .map(str::to_string),
        );
    }
    // Hatch names each package's own folder: its root is the one it is in.
    for package in strings(at(&["hatch", "build", "targets", "wheel", "packages"])) {
        let package = package.trim_end_matches('/');
        if let Some((folder, _)) = package.rsplit_once('/') {
            roots.push(folder.to_string());
        }
    }
    roots.extend(strings(at(&["pdm", "build", "package-dir"])));
    roots.extend(strings(at(&["maturin", "python-source"])));
    // pytest's ini-style `pythonpath` may be one string of space-separated
    // folders.
    for path in strings(at(&["pytest", "ini_options", "pythonpath"])) {
        roots.extend(path.split_whitespace().map(str::to_string));
    }
    for tool in ["pyright", "basedpyright"] {
        roots.extend(strings(at(&[tool, "extraPaths"])));
    }
    for path in strings(at(&["mypy", "mypy_path"])) {
        roots.extend(
            path.split([',', ':'])
                .map(str::trim)
                .filter(|p| !p.is_empty() && !p.contains('$'))
                .map(str::to_string),
        );
    }
    let mut out: Vec<String> = Vec::new();
    for root in roots {
        let root = root.trim().trim_start_matches("./").trim_end_matches('/');
        if root.is_empty() || root == "." || !plain_relative(root) {
            continue;
        }
        if !out.iter().any(|r| r == root) {
            out.push(root.to_string());
        }
    }
    out
}

/// A path written in a manifest that names a place under the manifest's
/// own folder: relative, and never climbing out of it.
fn plain_relative(path: &str) -> bool {
    let path = Path::new(path);
    !path.has_root()
        && path.components().all(|c| {
            matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
}

/// The source directories the Cabal package description at `path` names:
/// every stanza's `hs-source-dirs:`, relative to the file's own directory.
/// Bounded and plain-file-only for the same reason as [`read_go_module`].
pub fn read_cabal_source_dirs(path: &Path) -> Vec<String> {
    crate::statefile::read_capped(path, MAX_METADATA_BYTES)
        .map(|text| cabal_source_dirs(&text))
        .unwrap_or_default()
}

/// [`read_cabal_source_dirs`] over the file's text. A field's value runs on
/// over the lines indented deeper than its name (`hs-source-dirs: src` and a
/// `, lib` under it), its entries split on commas and spaces; field names are
/// case-insensitive, and the old singular `hs-source-dir:` counts too.
fn cabal_source_dirs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let indent = line.len() - line.trim_start().len();
        let Some((name, value)) = line.trim().split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        if name != "hs-source-dirs" && name != "hs-source-dir" {
            continue;
        }
        let mut value = value.to_string();
        while let Some(next) = lines.peek() {
            let deeper = next.len() - next.trim_start().len() > indent;
            if next.trim().is_empty() || !deeper {
                break;
            }
            // A comment line inside the value is no part of it.
            if !next.trim().starts_with("--") {
                value.push(' ');
                value.push_str(next.trim());
            }
            lines.next();
        }
        out.extend(
            value
                .split(|c: char| c == ',' || c.is_whitespace())
                .map(|dir| dir.trim_matches('"'))
                .filter(|dir| !dir.is_empty())
                .map(str::to_string),
        );
    }
    out
}

/// The source directories the hpack `package.yaml` at `path` names: every
/// `source-dirs:` (the package's own, or a component's), relative to the
/// file's own directory. Bounded and plain-file-only for the same reason as
/// [`read_go_module`].
pub fn read_hpack_source_dirs(path: &Path) -> Vec<String> {
    crate::statefile::read_capped(path, MAX_METADATA_BYTES)
        .map(|text| hpack_source_dirs(&text))
        .unwrap_or_default()
}

/// [`read_hpack_source_dirs`] over the file's text: a key's value is one
/// directory (`source-dirs: src`), a flow list (`[src, lib]`), or the `- `
/// items of a block list indented under it.
fn hpack_source_dirs(text: &str) -> Vec<String> {
    // A trailing comment is no part of a value.
    let uncomment = |s: &str| s.split(" #").next().unwrap_or_default().trim().to_string();
    let unquote = |s: &str| uncomment(s).trim_matches(['"', '\'']).to_string();
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let indent = line.len() - line.trim_start().len();
        let Some(value) = line.trim().strip_prefix("source-dirs:") else {
            continue;
        };
        let value = uncomment(value);
        if let Some(list) = value.strip_prefix('[') {
            let list = list.trim_end_matches(']');
            out.extend(list.split(',').map(unquote).filter(|d| !d.is_empty()));
        } else if !value.is_empty() {
            out.push(unquote(&value));
        } else {
            while let Some(next) = lines.peek() {
                let deeper = next.len() - next.trim_start().len() >= indent;
                let Some(item) = next.trim().strip_prefix("- ").filter(|_| deeper) else {
                    break;
                };
                out.push(unquote(item));
                lines.next();
            }
        }
    }
    out
}

fn node_text<'a>(node: Node, src: &'a str) -> &'a str {
    src.get(node.byte_range()).unwrap_or("")
}

fn line_of(node: Node) -> usize {
    node.start_position().row + 1
}

/// Run `query` against `root`, calling `f` once per match with that match's
/// captures resolved to `(capture_name, node)`.
fn for_each_match<'tree>(
    root: Node<'tree>,
    query: &Query,
    src: &str,
    mut f: impl FnMut(&[(&str, Node<'tree>)]),
) {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    // No import starts deeper: the cursor keeps a state per enclosing
    // pattern in progress, which cost depth² over a deeply nested file.
    cursor.set_max_start_depth(Some(MAX_IMPORT_DEPTH));
    let mut matches = cursor.matches(query, root, src.as_bytes());
    while let Some(m) = matches.next() {
        let caps: Vec<(&str, Node)> = m
            .captures
            .iter()
            .map(|c| (names[c.index as usize], c.node))
            .collect();
        f(&caps);
    }
}

/// Deepest an import is found at, in tree levels: real code imports at the
/// top or a few blocks down.
const MAX_IMPORT_DEPTH: u32 = 256;

fn capture<'t>(caps: &[(&str, Node<'t>)], name: &str) -> Option<Node<'t>> {
    caps.iter().find(|(n, _)| *n == name).map(|(_, node)| *node)
}

const RUST_QUERY: &str = r#"
(mod_item name: (identifier) @mod_name) @mod_item
(use_declaration argument: (_) @use_arg) @use_decl
"#;

fn rust_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_rust_import(root, src, query, |raw, _| out.push(raw));
    out
}

/// Extract an import together with the declaration node that owns it. Keeping
/// the node until scoping avoids collapsing separate declarations on one line.
pub(crate) fn for_each_rust_import<'tree>(
    root: Node<'tree>,
    src: &str,
    query: &Query,
    mut emit: impl FnMut(RawImport, Node<'tree>),
) {
    // What is left of the file's import budget (see `MAX_RUST_IMPORTS`).
    let mut budget = MAX_RUST_IMPORTS;
    for_each_match(root, query, src, |caps| {
        if let Some(mi) = capture(caps, "mod_item") {
            // `mod x { ... }` (has a body) is an inline module, not a file edge.
            if mi.child_by_field_name("body").is_some() {
                return;
            }
            if let Some(name) = capture(caps, "mod_name") {
                // A declaration is an import too: scoping copies the inline
                // modules around it into each one (`rustscope`).
                if budget == 0 {
                    return;
                }
                budget -= 1;
                // A `#[path = "…"]` says which file the module is: it goes
                // with the name (see `rustscope::PATH_ATTR`).
                let module = match path_attribute(mi, src) {
                    Some(path) => format!(
                        "{}{}{path}",
                        node_text(name, src),
                        crate::rustscope::PATH_ATTR
                    ),
                    None => node_text(name, src).to_string(),
                };
                emit(
                    RawImport {
                        module,
                        line: line_of(mi),
                        is_mod_decl: true,
                    },
                    mi,
                );
            }
            return;
        }
        if let Some(arg) = capture(caps, "use_arg") {
            // The declaration comes from the match, not `arg.parent()`:
            // tree-sitter finds a parent by walking down from the root, a
            // cost of the depth per import.
            let reexport = capture(caps, "use_decl").is_some_and(|decl| is_reexport(decl, src));
            let paths = expand_use_tree(arg, src, budget);
            budget -= paths.len();
            for (module, line) in paths {
                emit(
                    RawImport {
                        module: if reexport {
                            format!("{}{module}", crate::rustscope::REEXPORT)
                        } else {
                            module
                        },
                        line,
                        is_mod_decl: false,
                    },
                    arg,
                );
            }
        }
    });
}

/// The file a `#[path = "…"]` among the attributes right above a module
/// declaration names, as written; `None` without one (or one under a
/// `cfg_attr`, which may not apply).
fn path_attribute(mod_item: Node, src: &str) -> Option<String> {
    let mut at = mod_item.prev_named_sibling();
    while let Some(node) = at {
        match node.kind() {
            "attribute_item" => {
                let text: String = node_text(node, src)
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                if let Some(rest) = text.strip_prefix("#[path=\"")
                    && let Some(path) = rest.strip_suffix("\"]")
                    && !path.is_empty()
                    && !path.contains(['"', '\\'])
                {
                    // Spaces in the path were squeezed out with the rest:
                    // read the literal itself.
                    let raw = node_text(node, src);
                    let open = raw.find('"')? + 1;
                    let close = raw.rfind('"')?;
                    return raw.get(open..close).map(str::to_string);
                }
            }
            "line_comment" | "block_comment" => {}
            _ => return None,
        }
        at = node.prev_named_sibling();
    }
    None
}

/// Whether a `use_declaration` re-exports what it names: it carries a
/// visibility wider than its own module (`pub(self)` is private).
fn is_reexport(decl: Node, src: &str) -> bool {
    let mut cursor = decl.walk();
    decl.children(&mut cursor)
        .any(|c| c.kind() == "visibility_modifier" && path_text(c, src) != "pub(self)")
}

/// Every path a `use` tree imports, in source order, each on its own line:
/// `use a::{b::c, d as e, f::*, self}` yields `a::b::c`, `a::d`, `a::f::*`
/// and `a` — a glob as the module it reads from plus its star, the path an
/// alias renames, the prefix `self` names.
///
/// A grouped import used to collapse to its common prefix, so
/// `use crate::{graph::layout, ui}` resolved to the crate ROOT rather than to
/// the two modules it actually names. The nesting is walked with an explicit
/// stack: it is repository-controlled, and deep enough nesting would
/// otherwise overflow the thread's stack.
///
/// The fan-out is bounded too: each entry of a list is its prefix plus the
/// entry, so `use <long path>::{b, b, b, …}` cost entries × prefix — tens of
/// GiB from one file within the index's size cap. The entries share their
/// prefix while queued, at most `budget` paths are returned, and a path
/// longer than [`MAX_RUST_IMPORT_LEN`] (no real one is) is left out.
fn expand_use_tree(arg: Node, src: &str, budget: usize) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut stack: Vec<(Node, std::rc::Rc<str>)> = vec![(arg, "".into())];
    while let Some((node, prefix)) = stack.pop() {
        if out.len() >= budget {
            break;
        }
        if prefix.len() > MAX_RUST_IMPORT_LEN {
            continue;
        }
        match node.kind() {
            "scoped_use_list" => {
                let path = node
                    .child_by_field_name("path")
                    .map(|p| path_text(p, src))
                    .unwrap_or_default();
                if let Some(list) = node.child_by_field_name("list") {
                    push_use_list(list, join_path(&prefix, &path).into(), &mut stack);
                }
            }
            "use_list" => push_use_list(node, prefix, &mut stack),
            "use_as_clause" => {
                if let Some(path) = node.child_by_field_name("path") {
                    stack.push((path, prefix));
                }
            }
            "use_wildcard" => {
                let path = node
                    .named_child(0)
                    .map(|p| path_text(p, src))
                    .unwrap_or_default();
                let module = join_path(&prefix, &path);
                if !module.is_empty() && module.len() <= MAX_RUST_IMPORT_LEN {
                    let glob = join_path(&module, crate::rustscope::GLOB);
                    out.push((glob, line_of(node)));
                }
            }
            // `use a::{self}`: the list's own prefix.
            "self" if !prefix.is_empty() => out.push((prefix.to_string(), line_of(node))),
            "line_comment" | "block_comment" => {}
            _ => {
                let module = join_path(&prefix, &path_text(node, src));
                if !module.is_empty() && module.len() <= MAX_RUST_IMPORT_LEN {
                    out.push((module, line_of(node)));
                }
            }
        }
    }
    out
}

/// Most import paths one file yields (a real file has some dozens) — named
/// for Rust, it bounds every language's list: each entry of a Rust, Python or
/// PHP list carries the list's prefix, so their count times the prefix is
/// what one file can cost.
const MAX_RUST_IMPORTS: usize = 10_000;

/// Longest import path kept, in bytes (in every language, as above).
pub(crate) const MAX_RUST_IMPORT_LEN: usize = 1024;

/// Queue a `use_list`'s entries so they pop in source order, sharing the
/// prefix.
fn push_use_list<'t>(
    list: Node<'t>,
    prefix: std::rc::Rc<str>,
    stack: &mut Vec<(Node<'t>, std::rc::Rc<str>)>,
) {
    let mut cursor = list.walk();
    let entries: Vec<Node> = list.named_children(&mut cursor).collect();
    for entry in entries.into_iter().rev() {
        stack.push((entry, prefix.clone()));
    }
}

/// A path node's text with any whitespace inside it removed (`a :: b`).
fn path_text(node: Node, src: &str) -> String {
    node_text(node, src)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

fn join_path(prefix: &str, path: &str) -> String {
    match (prefix.is_empty(), path.is_empty()) {
        (true, _) => path.to_string(),
        (false, true) => prefix.to_string(),
        (false, false) => format!("{prefix}::{path}"),
    }
}

/// How `raw` reads as the statement that wrote it — for a model or a person,
/// not for the resolver: extraction's encodings decoded. A Rust re-export's
/// `pub ` and a glob's `::*` become `pub use a::b::*`, a module declaration
/// `mod x`; Python's `.:views` becomes `from . import views`; a Java or C#
/// declaration `package a.b` or `namespace A.B`, so it is not read as an
/// import; an Erlang `lib:a/x.hrl` `-include_lib("a/x.hrl")`. Other
/// languages' specifiers read as written.
pub fn statement_of(raw: &RawImport, lang: Lang) -> String {
    match lang {
        Lang::Java if raw.is_mod_decl => format!("package {}", raw.module),
        Lang::CSharp if raw.is_mod_decl => format!("namespace {}", raw.module),
        Lang::Erlang => match raw.module.strip_prefix(ERLANG_LIB) {
            Some(path) => format!("-include_lib(\"{path}\")"),
            None => raw.module.clone(),
        },
        Lang::Rust if raw.is_mod_decl => match crate::rustscope::mod_decl_parts(&raw.module) {
            (name, Some(path)) => format!("#[path = \"{path}\"] mod {name}"),
            (name, None) => format!("mod {name}"),
        },
        Lang::Rust => {
            let u = crate::rustscope::RustUse::parse(&raw.module);
            let glob = match (u.glob, u.path.is_empty()) {
                (true, true) => "*",
                (true, false) => "::*",
                (false, _) => "",
            };
            let vis = if u.reexport { "pub " } else { "" };
            format!("{vis}use {}{glob}", u.path)
        }
        Lang::Python => match raw.module.split_once(PY_NAME_SEP) {
            Some((package, name)) => format!("from {package} import {name}"),
            None => raw.module.clone(),
        },
        _ => raw.module.clone(),
    }
}

/// Separates a module from a name imported out of it, for `from . import
/// views` (`.:views`), `from .. import up` (`..:up`) and `from shop import
/// payments` (`shop:payments`): the name is the package's submodule when
/// one exists, else an attribute of the module (a package's `__init__.py`).
/// Never part of a module path.
pub const PY_NAME_SEP: char = ':';

const PYTHON_QUERY: &str = r#"
(import_statement name: (dotted_name) @plain)
(import_statement name: (aliased_import name: (dotted_name) @plain))
(import_from_statement module_name: (_) @from) @stmt
"#;

fn python_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        // Bounded as Rust's are (`MAX_RUST_IMPORTS`): `from <dots> import a,
        // b, …` gives each name the dots, which a file within the index's
        // size cap can make tens of GiB.
        if out.len() >= MAX_RUST_IMPORTS {
            return;
        }
        if let Some(plain) = capture(caps, "plain") {
            push_module(&mut out, node_text(plain, src).trim(), line_of(plain));
            return;
        }
        let (Some(module), Some(stmt)) = (capture(caps, "from"), capture(caps, "stmt")) else {
            return;
        };
        let text = node_text(module, src).trim();
        // `from . import views` imports the NAME `views` of the current
        // package: its submodule `views.py` when there is one, else whatever
        // its `__init__.py` defines under that name. Recording just `.` pointed
        // every such edge at the package even for a submodule; recording
        // `.views` made a name `__init__.py` defines unresolvable. `.:views`
        // (see [`PY_NAME_SEP`]) keeps both readings for the resolver. So does
        // `from shop import payments` (`shop:payments`), whose edge went to
        // `shop/__init__.py` though `shop/payments.py` is what it reads; in
        // `from shop.models import Tea` the name is no submodule, and the
        // edge is the module's.
        if !text.is_empty() {
            let mut cursor = stmt.walk();
            let names: Vec<Node> = stmt.children_by_field_name("name", &mut cursor).collect();
            for name in &names {
                let dotted = match name.kind() {
                    "aliased_import" => name.child_by_field_name("name"),
                    _ => Some(*name),
                };
                if let Some(dotted) = dotted {
                    let name = node_text(dotted, src).trim();
                    if out.len() >= MAX_RUST_IMPORTS
                        || text.len() + name.len() >= MAX_RUST_IMPORT_LEN
                    {
                        continue;
                    }
                    push_module(
                        &mut out,
                        &format!("{text}{PY_NAME_SEP}{name}"),
                        line_of(dotted),
                    );
                }
            }
            if !names.is_empty() {
                return;
            }
        }
        // `from x import y` depends on `x`; `from . import *` on the package.
        push_module(&mut out, text, line_of(module));
    });
    out
}

fn push_module(out: &mut Vec<RawImport>, module: &str, line: usize) {
    if !module.is_empty() {
        out.push(RawImport {
            module: module.to_string(),
            line,
            is_mod_decl: false,
        });
    }
}

/// The JavaScript import patterns, shared verbatim by TypeScript.
macro_rules! js_import_patterns {
    () => {
        r#"
(import_statement source: (string) @src)
(export_statement source: (string) @src)
(call_expression
  function: (identifier) @fn
  arguments: (arguments . (string) @src))
(call_expression
  function: (import)
  arguments: (arguments . (string) @src))
"#
    };
}

/// ES module imports and re-exports, CommonJS `require("…")`, and dynamic
/// `import("…")`. The two call forms used to be missed entirely, so a
/// CommonJS codebase showed no import graph at all. Only a string-literal
/// argument names a module; `require(name)` is data, not an edge.
const JS_QUERY: &str = js_import_patterns!();

/// [`JS_QUERY`] plus TypeScript's `import x = require("…")`.
const TS_QUERY: &str = concat!(
    js_import_patterns!(),
    "(import_require_clause source: (string) @src)\n"
);

fn js_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        // The call form captures its callee; only `require` imports.
        if let Some(callee) = capture(caps, "fn")
            && node_text(callee, src) != "require"
        {
            return;
        }
        if let Some(node) = capture(caps, "src") {
            push_module(&mut out, strip_quotes(node_text(node, src)), line_of(node));
        }
    });
    out
}

const GO_QUERY: &str = r#"
(import_spec path: (interpreted_string_literal) @path)
(import_spec path: (raw_string_literal) @path)
"#;

// Dart `import`/`export` both carry the target as a `uri (string_literal)`,
// either directly or wrapped in a `configurable_uri` (for `if (dart.library.io)`
// conditional imports).
const DART_QUERY: &str = r#"
(library_import (import_specification (uri (string_literal) @path)))
(library_import (import_specification (configurable_uri (uri (string_literal) @path))))
(library_export (configurable_uri (uri (string_literal) @path)))
"#;

/// Go and Dart: one quoted path per `@path` capture.
fn path_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        if let Some(node) = capture(caps, "path") {
            push_module(&mut out, strip_quotes(node_text(node, src)), line_of(node));
        }
    });
    out
}

// Lean `import A.B.C` (`public import`, `import all` and `meta import` too):
// one module, a dotted name the grammar keeps as several `module` nodes.
const LEAN_QUERY: &str = r#"
(import) @import
"#;

/// Lean: one module per `import`, read whole across its dotted parts.
fn lean_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        let Some(node) = capture(caps, "import") else {
            return;
        };
        let mut cursor = node.walk();
        let parts: Vec<Node> = node.children_by_field_name("module", &mut cursor).collect();
        if let (Some(first), Some(last)) = (parts.first(), parts.last())
            && let Some(module) = src.get(first.start_byte()..last.end_byte())
        {
            push_module(&mut out, module, line_of(node));
        }
    });
    out
}

/// A declaration (see [`RawImport::is_mod_decl`]): what the file says it is
/// part of, not what it imports.
fn push_declaration(out: &mut Vec<RawImport>, module: &str, line: usize) {
    if !module.is_empty() {
        out.push(RawImport {
            module: module.to_string(),
            line,
            is_mod_decl: true,
        });
    }
}

/// A string literal's value, when the literal writes it out whole: its one
/// `string_content` (PHP, Ruby, Zig), or — in a grammar that keeps no
/// content node (Erlang) — the text between its quotes. `None` for one with
/// interpolation or escapes in it, whose value is not there as written.
fn literal_text<'a>(node: Node, src: &'a str) -> Option<&'a str> {
    match node.named_child_count() {
        0 => {
            let text = node_text(node, src).trim();
            let inner = strip_quotes(text);
            (inner.len() + 2 == text.len()).then_some(inner)
        }
        1 => node
            .named_child(0)
            .filter(|c| c.kind() == "string_content")
            .map(|c| node_text(c, src)),
        _ => None,
    }
}

// Java `package a.b;` (a declaration: see `RawImport::is_mod_decl`) and every
// `import`, static or not, a star one too. The name is read off the
// declaration's children: an annotation can come before a package's name,
// and a star import ends in an `asterisk` beside it.
const JAVA_QUERY: &str = r#"
(package_declaration) @package
(import_declaration) @import
"#;

/// Java: the file's package, as a declaration, and one specifier per import —
/// `a.b.C`, `a.b.*` for a star, `a.b.C.m` for a static member (which the
/// resolver finds through its class, as it finds a nested class).
fn java_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        if out.len() >= MAX_RUST_IMPORTS {
            return;
        }
        let (decl, is_package) = match (capture(caps, "package"), capture(caps, "import")) {
            (Some(decl), _) => (decl, true),
            (None, Some(decl)) => (decl, false),
            (None, None) => return,
        };
        let mut cursor = decl.walk();
        let mut name = None;
        let mut star = false;
        for child in decl.named_children(&mut cursor) {
            match child.kind() {
                "scoped_identifier" | "identifier" => name = Some(child),
                "asterisk" => star = true,
                _ => {}
            }
        }
        // The name's line: an annotation above `package` starts the node.
        let Some(node) = name else {
            return;
        };
        let mut name = path_text(node, src);
        if name.len() > MAX_RUST_IMPORT_LEN {
            return;
        }
        if is_package {
            push_declaration(&mut out, &name, line_of(node));
            return;
        }
        if star {
            name.push_str(".*");
        }
        push_module(&mut out, &name, line_of(node));
    });
    out
}

// Kotlin: the file's package, as a declaration, and one specifier per
// import, as Java's (`a.b.C`, `a.b.*`); an alias (`as D`) is the same
// import.
const KOTLIN_QUERY: &str = r#"
(package_header (qualified_identifier) @package)
(import (qualified_identifier) @name) @import
"#;

fn kotlin_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        if out.len() >= MAX_RUST_IMPORTS {
            return;
        }
        if let Some(package) = capture(caps, "package") {
            let name = path_text(package, src);
            if name.len() <= MAX_RUST_IMPORT_LEN {
                push_declaration(&mut out, &name, line_of(package));
            }
            return;
        }
        let (Some(import), Some(name)) = (capture(caps, "import"), capture(caps, "name")) else {
            return;
        };
        let mut spec = path_text(name, src);
        if spec.len() > MAX_RUST_IMPORT_LEN {
            return;
        }
        // `import a.b.*`: the star is no node of its own.
        let after = src.get(name.end_byte()..import.end_byte()).unwrap_or("");
        if after.trim_start().starts_with(".*") {
            spec.push_str(".*");
        }
        push_module(&mut out, &spec, line_of(name));
    });
    out
}

// Scala: the package its `package` lines chain to (`package a.b` then
// `package c` is `a.b.c`), as a declaration, and one specifier per name an
// import brings in, as Java's: `a.b.C`, `a.b.*` for a wildcard (`_` or
// `*`), each of `a.b.{C, D => E}`'s.
const SCALA_QUERY: &str = r#"
(compilation_unit (package_clause name: (package_identifier) @package))
(import_declaration) @import
"#;

fn scala_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    let mut package: Option<(String, usize)> = None;
    for_each_match(root, query, src, |caps| {
        if out.len() >= MAX_RUST_IMPORTS {
            return;
        }
        if let Some(clause) = capture(caps, "package") {
            let name = path_text(clause, src);
            package = Some(match package.take() {
                Some((before, line)) => (format!("{before}.{name}"), line),
                None => (name, line_of(clause)),
            });
            return;
        }
        let Some(import) = capture(caps, "import") else {
            return;
        };
        let mut cursor = import.walk();
        let base: Vec<&str> = import
            .children_by_field_name("path", &mut cursor)
            .filter(|p| p.is_named())
            .map(|p| node_text(p, src))
            .collect();
        let base = base.join(".");
        if base.is_empty() || base.len() > MAX_RUST_IMPORT_LEN {
            return;
        }
        let line = line_of(import);
        let mut cursor = import.walk();
        let mut named = false;
        for child in import.named_children(&mut cursor) {
            match child.kind() {
                "namespace_wildcard" => {
                    push_module(&mut out, &format!("{base}.*"), line);
                    named = true;
                }
                "namespace_selectors" => {
                    named = true;
                    let mut inner = child.walk();
                    for selector in child.named_children(&mut inner) {
                        let name = match selector.kind() {
                            "identifier" => Some(selector),
                            "arrow_renamed_identifier" | "as_renamed_identifier" => {
                                selector.child_by_field_name("name")
                            }
                            "namespace_wildcard" => {
                                push_module(&mut out, &format!("{base}.*"), line);
                                None
                            }
                            _ => None,
                        };
                        if let Some(name) = name {
                            push_module(
                                &mut out,
                                &format!("{base}.{}", node_text(name, src)),
                                line,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        if !named {
            push_module(&mut out, &base, line);
        }
    });
    if let Some((name, line)) = package
        && name.len() <= MAX_RUST_IMPORT_LEN
    {
        push_declaration(&mut out, &name, line);
    }
    out
}

// Elixir: the module each `alias`, `import`, `use` and `require` names
// (`A.B`, each of `alias A.{B, C}`'s), and each module the file defines,
// by its full name (`defmodule A do defmodule B` defines `A` and `A.B`), as
// declarations.
const ELIXIR_QUERY: &str = r#"
(call
  target: (identifier) @kw
  (arguments . (alias) @module)
  (#any-of? @kw "alias" "import" "use" "require"))
(call
  target: (identifier) @kw
  (arguments . (dot left: (alias) @base right: (tuple) @group))
  (#any-of? @kw "alias" "import" "use" "require"))
(call
  target: (identifier) @kw
  (arguments . (alias) @defines)
  (#any-of? @kw "defmodule" "defprotocol")) @definition
"#;

fn elixir_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        if out.len() >= MAX_RUST_IMPORTS {
            return;
        }
        if let Some(module) = capture(caps, "module") {
            push_module(&mut out, node_text(module, src), line_of(module));
        } else if let (Some(base), Some(group)) = (capture(caps, "base"), capture(caps, "group")) {
            let base = node_text(base, src);
            let mut cursor = group.walk();
            for member in group
                .named_children(&mut cursor)
                .filter(|m| m.kind() == "alias")
            {
                push_module(
                    &mut out,
                    &format!("{base}.{}", node_text(member, src)),
                    line_of(member),
                );
            }
        } else if let (Some(name), Some(definition)) =
            (capture(caps, "defines"), capture(caps, "definition"))
        {
            // Nested in another `defmodule`: named under it.
            let mut full = node_text(name, src).to_string();
            let mut node = definition.parent();
            while let Some(n) = node {
                if n.kind() == "call"
                    && n.child_by_field_name("target")
                        .is_some_and(|t| matches!(node_text(t, src), "defmodule" | "defprotocol"))
                    && let Some(outer) = n
                        .named_children(&mut n.walk())
                        .find(|c| c.kind() == "arguments")
                        .and_then(|a| a.named_child(0))
                        .filter(|a| a.kind() == "alias")
                {
                    full = format!("{}.{full}", node_text(outer, src));
                }
                node = n.parent();
            }
            if full.len() <= MAX_RUST_IMPORT_LEN {
                push_declaration(&mut out, &full, line_of(name));
            }
        }
    });
    out
}

// C# `using` directives (`global`, `static` and aliased ones too) and the
// namespaces a file declares, block-bodied or file-scoped (declarations: see
// `RawImport::is_mod_decl`).
const CSHARP_QUERY: &str = r#"
(using_directive) @using
(namespace_declaration name: (_) @name) @namespace
(file_scoped_namespace_declaration name: (_) @name) @namespace
"#;

/// C#: one specifier per `using` — the namespace or type it names, whatever
/// it is brought in as — and each namespace the file declares, by its full
/// name: `namespace A { namespace B { … } }` declares `A` and `A.B`.
fn csharp_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    // Each namespace declaration with its own name, nested ones joined to
    // their enclosing ones' below, once every declaration is in.
    let mut spaces: Vec<(Node, String)> = Vec::new();
    for_each_match(root, query, src, |caps| {
        if out.len() + spaces.len() >= MAX_RUST_IMPORTS {
            return;
        }
        if let (Some(decl), Some(name)) = (capture(caps, "namespace"), capture(caps, "name")) {
            if let Some(name) = csharp_name(&path_text(name, src)) {
                spaces.push((decl, name));
            }
            return;
        }
        let Some(using) = capture(caps, "using") else {
            return;
        };
        // What a `using` brings in is its last named child: `using X = A.B;`
        // names its alias (the `name` field) first.
        let alias = using.child_by_field_name("name");
        let mut cursor = using.walk();
        let target = using
            .named_children(&mut cursor)
            .filter(|c| Some(*c) != alias && !c.is_extra())
            .last();
        if let Some(name) = target.and_then(|t| csharp_name(&path_text(t, src))) {
            push_module(&mut out, &name, line_of(using));
        }
    });
    // The declarations enclosing one start before it and end after it: a
    // stack of the open ones, in source order, gives each its full name. One
    // whose full name grows past the length cap is left out, and so is
    // everything nested in it.
    spaces.sort_by_key(|(decl, _)| decl.start_byte());
    let mut open: Vec<(usize, Option<String>)> = Vec::new();
    for (decl, name) in spaces {
        while open
            .last()
            .is_some_and(|(end, _)| *end <= decl.start_byte())
        {
            open.pop();
        }
        let full = match open.last() {
            Some((_, Some(outer))) => Some(format!("{outer}.{name}")),
            Some((_, None)) => None,
            None => Some(name),
        }
        .filter(|full| full.len() <= MAX_RUST_IMPORT_LEN);
        if let Some(full) = &full {
            push_declaration(&mut out, full, line_of(decl));
        }
        open.push((decl.end_byte(), full));
    }
    out
}

/// A C# namespace-or-type name as the resolver reads it: `global::` and the
/// `@` of a verbatim identifier dropped, type arguments cut off (`List<int>`
/// is `List`). `None` for anything else a `using` alias can name (a pointer
/// or a tuple type, an extern alias's `X::A`).
fn csharp_name(text: &str) -> Option<String> {
    let text = text.strip_prefix("global::").unwrap_or(text);
    let name: String = text
        .split('<')
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|&c| c != '@')
        .collect();
    let valid = name.split('.').all(|segment| {
        !segment.is_empty() && segment.chars().all(|c| c.is_alphanumeric() || c == '_')
    });
    valid.then_some(name)
}

// PHP `use` declarations — a group's clauses each under the group's prefix,
// functions and constants as classes are — and `require`/`include` (their
// `_once` forms too) of a path the expression writes down.
const PHP_QUERY: &str = r#"
(namespace_use_declaration) @use
(require_expression) @require
(require_once_expression) @require
(include_expression) @require
(include_once_expression) @require
"#;

/// PHP: one qualified name per `use` clause (`A\B\C`, no leading backslash,
/// its alias dropped), and one path per `require`/`include` of one.
fn php_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        if out.len() >= MAX_RUST_IMPORTS {
            return;
        }
        if let Some(decl) = capture(caps, "use") {
            php_use(decl, src, &mut out);
        } else if let Some(expr) = capture(caps, "require")
            && let Some(path) = expr.named_child(0).and_then(|arg| php_path(arg, src))
        {
            push_module(&mut out, &path, line_of(expr));
        }
    });
    out
}

/// The names one PHP `use` declaration brings in: `use A\B, C` names `A\B`
/// and `C`; `use A\B\{C, D as E}` names `A\B\C` and `A\B\D`. Bounded as a
/// Rust group is ([`MAX_RUST_IMPORTS`], [`MAX_RUST_IMPORT_LEN`]): each entry
/// of a group carries its prefix.
fn php_use(decl: Node, src: &str, out: &mut Vec<RawImport>) {
    let php_name = |node: Node| path_text(node, src).trim_start_matches('\\').to_string();
    let mut cursor = decl.walk();
    let children: Vec<Node> = decl.named_children(&mut cursor).collect();
    let prefix = children
        .iter()
        .find(|c| matches!(c.kind(), "namespace_name" | "qualified_name" | "name"))
        .map(|p| php_name(*p))
        .unwrap_or_default();
    let mut clauses = Vec::new();
    for child in children {
        match child.kind() {
            "namespace_use_clause" => clauses.push(child),
            "namespace_use_group" => {
                let mut cursor = child.walk();
                clauses.extend(
                    child
                        .named_children(&mut cursor)
                        .filter(|c| c.kind() == "namespace_use_clause"),
                );
            }
            _ => {}
        }
    }
    for clause in clauses {
        if out.len() >= MAX_RUST_IMPORTS {
            return;
        }
        let alias = clause.child_by_field_name("alias");
        let mut cursor = clause.walk();
        let Some(name) = clause.named_children(&mut cursor).find(|n| {
            Some(*n) != alias && matches!(n.kind(), "name" | "qualified_name" | "namespace_name")
        }) else {
            continue;
        };
        let name = php_name(name);
        if prefix.len() + name.len() >= MAX_RUST_IMPORT_LEN {
            continue;
        }
        let full = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}\\{name}")
        };
        push_module(out, &full, line_of(clause));
    }
}

/// The path a `require`/`include` names, when it writes one down: a string
/// literal, or one appended to the file's own directory — `__DIR__ . '/x'`
/// and `dirname(__FILE__) . '/x'` (both `__DIR__/x`), `dirname(__DIR__) .
/// '/x'` (`__DIR__/../x`). A path computed any other way is data, not an
/// edge.
fn php_path(arg: Node, src: &str) -> Option<String> {
    let mut arg = arg;
    while arg.kind() == "parenthesized_expression" {
        arg = arg.named_child(0)?;
    }
    let literal = |node: Node| {
        matches!(node.kind(), "string" | "encapsed_string")
            .then(|| literal_text(node, src))
            .flatten()
    };
    let path = match arg.kind() {
        "binary_expression" => {
            let operator = arg.child_by_field_name("operator")?;
            if node_text(operator, src) != "." {
                return None;
            }
            let dir = php_dir(arg.child_by_field_name("left")?, src)?;
            format!("{dir}{}", literal(arg.child_by_field_name("right")?)?)
        }
        _ => literal(arg)?.to_string(),
    };
    (path.len() <= MAX_RUST_IMPORT_LEN).then_some(path)
}

/// The directory a PHP path expression starts from, when it is the file's
/// own (`__DIR__`, `dirname(__FILE__)`) or its parent (`dirname(__DIR__)`).
fn php_dir(node: Node, src: &str) -> Option<&'static str> {
    match node.kind() {
        "name" if node_text(node, src) == "__DIR__" => Some("__DIR__"),
        "function_call_expression" => {
            let function = node.child_by_field_name("function")?;
            if node_text(function, src) != "dirname" {
                return None;
            }
            let args = node.child_by_field_name("arguments")?;
            if args.named_child_count() != 1 {
                return None;
            }
            match node_text(args.named_child(0)?, src).trim() {
                "__FILE__" => Some("__DIR__"),
                "__DIR__" => Some("__DIR__/.."),
                _ => None,
            }
        }
        _ => None,
    }
}

// Ruby `require`, `require_relative` and `load` of a string literal, called
// as plain methods (no receiver); which of the three it is, the callee says.
const RUBY_QUERY: &str = r#"
(call
  method: (identifier) @fn
  arguments: (argument_list . (string) @path)
  !receiver)
"#;

/// Ruby: a `require` or `load` as written (`x`, a load-path feature), a
/// `require_relative` as the relative path it is (`./x`, `../x`).
fn ruby_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        if out.len() >= MAX_RUST_IMPORTS {
            return;
        }
        let (Some(callee), Some(path)) = (capture(caps, "fn"), capture(caps, "path")) else {
            return;
        };
        let Some(text) = literal_text(path, src) else {
            return;
        };
        let module = match node_text(callee, src) {
            "require" | "load" => text.to_string(),
            "require_relative" if text.starts_with('.') || text.starts_with('/') => {
                text.to_string()
            }
            "require_relative" => format!("./{text}"),
            _ => return,
        };
        push_module(&mut out, &module, line_of(path));
    });
    out
}

/// Prefix of an Erlang `-include_lib("app/include/x.hrl")`'s specifier: the
/// path's first part names an application, found wherever the project keeps
/// it (`apps/`, `deps/`, `_build/…/lib/`) or else another package's, where a
/// plain `-include`'s path is the importer's own to find. No path an
/// `-include` writes begins with it.
pub const ERLANG_LIB: &str = "lib:";

// Erlang headers (`-include`, `-include_lib`: a path in one string, or in
// several written side by side) and the modules a file names as its
// behaviour (`-behaviour`, `-behavior`) or imports functions from (`-import`).
// A remote call's module (`lists:map(…)`) is a call, not an import.
const ERLANG_QUERY: &str = r#"
(pp_include) @include
(pp_include_lib) @include_lib
(import_attribute module: (atom) @module)
(behaviour_attribute name: (atom) @module)
"#;

/// Erlang: a module by name (`gen_server`, unquoted), a header by its path
/// (`x.hrl`; an `-include_lib`'s prefixed [`ERLANG_LIB`]).
fn erlang_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        if out.len() >= MAX_RUST_IMPORTS {
            return;
        }
        if let Some(module) = capture(caps, "module") {
            let name = node_text(module, src).trim().trim_matches('\'');
            push_module(&mut out, name, line_of(module));
            return;
        }
        let (attr, lib) = match (capture(caps, "include"), capture(caps, "include_lib")) {
            (Some(attr), _) => (attr, false),
            (None, Some(attr)) => (attr, true),
            (None, None) => return,
        };
        // `-include("a" "b.hrl")` is one path, its strings joined; one with a
        // macro in it (`?DIR "/x.hrl"`) is not written down.
        let mut cursor = attr.walk();
        let mut path = String::new();
        for part in attr.children_by_field_name("file", &mut cursor) {
            match (part.kind(), literal_text(part, src)) {
                ("string", Some(text)) => path.push_str(text),
                _ => return,
            }
        }
        if path.len() > MAX_RUST_IMPORT_LEN {
            return;
        }
        if lib && !path.is_empty() {
            path.insert_str(0, ERLANG_LIB);
        }
        push_module(&mut out, &path, line_of(attr));
    });
    out
}

// Haskell `import`s — `qualified`, aliased, with an import list or a package
// name (`import "pkg" A.B`) alike. The module imported is the `module` field;
// an alias is a module node too, under another field.
const HASKELL_QUERY: &str = r#"
(import module: (module) @module)
"#;

/// Haskell: one module per `import` (`Data.Map`).
fn haskell_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        if out.len() < MAX_RUST_IMPORTS
            && let Some(module) = capture(caps, "module")
        {
            push_module(&mut out, &path_text(module, src), line_of(module));
        }
    });
    out
}

// Zig `@import` of a string literal — a file (`x.zig`) or a package (`std`,
// `builtin`, a `build.zig.zon` dependency); another builtin (`@embedFile`)
// is not an import.
const ZIG_QUERY: &str = r#"
(builtin_function (builtin_identifier) @fn (arguments . (string) @path))
"#;

/// Zig: what each `@import` names, as written.
fn zig_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        if out.len() >= MAX_RUST_IMPORTS {
            return;
        }
        let (Some(callee), Some(path)) = (capture(caps, "fn"), capture(caps, "path")) else {
            return;
        };
        if node_text(callee, src) == "@import"
            && let Some(text) = literal_text(path, src)
        {
            push_module(&mut out, text, line_of(path));
        }
    });
    out
}

// C's and C++'s `#include "cart.hpp"` and `#include <vector>`; an include
// of a macro (`#include CONFIG_H`) names no file as written.
const C_QUERY: &str = r#"
(preproc_include path: [(string_literal) (system_lib_string)] @path)
"#;

/// C and C++: each `#include`, a quoted one as its path (`cart.hpp`), an
/// angle-bracketed one with its brackets (`<vector>`), which the resolver
/// looks for in the include path only, never beside the includer.
fn c_includes(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        if out.len() >= MAX_RUST_IMPORTS {
            return;
        }
        let Some(path) = capture(caps, "path") else {
            return;
        };
        let module = match path.kind() {
            "system_lib_string" => Some(node_text(path, src).trim()),
            _ => literal_text(path, src).map(str::trim),
        };
        if let Some(module) = module.filter(|m| !matches!(*m, "" | "<>")) {
            push_module(&mut out, module, line_of(path));
        }
    });
    out
}

/// Strip a single pair of surrounding `"`, `'` or backtick quotes.
fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'' || first == b'`') && first == last {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// JSON with comments (`//`, `/* */`) and trailing commas → strict JSON, the
/// dialect tsconfig files are written in. String contents are left alone.
pub fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut last = 0; // start of the pending verbatim run
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            match b {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                out.push_str(&text[last..i]);
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                last = i;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                out.push_str(&text[last..i]);
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                last = i;
            }
            b',' => {
                // A comma whose next significant character closes the
                // container is a trailing comma: drop it.
                let mut j = i + 1;
                loop {
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b'/') && bytes.get(j + 1) == Some(&b'/') {
                        while j < bytes.len() && bytes[j] != b'\n' {
                            j += 1;
                        }
                    } else if bytes.get(j) == Some(&b'/') && bytes.get(j + 1) == Some(&b'*') {
                        j += 2;
                        while j < bytes.len()
                            && !(bytes[j] == b'*' && bytes.get(j + 1) == Some(&b'/'))
                        {
                            j += 1;
                        }
                        j = (j + 2).min(bytes.len());
                    } else {
                        break;
                    }
                }
                if matches!(bytes.get(j), Some(b'}' | b']')) {
                    out.push_str(&text[last..i]);
                    last = i + 1;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod extraction_tests {
    use super::*;

    /// A19: the explain prompt's import list reads as the code wrote it —
    /// none of the extraction's encodings (`pub `, `::*`, `.:name`) reach the
    /// model as if they were syntax.
    #[test]
    fn an_import_reads_as_the_statement_that_wrote_it() {
        let raw = |module: &str, is_mod_decl: bool| RawImport {
            module: module.into(),
            line: 1,
            is_mod_decl,
        };
        assert_eq!(
            statement_of(&raw("pub app::model::*", false), Lang::Rust),
            "pub use app::model::*"
        );
        assert_eq!(
            statement_of(&raw("super::*", false), Lang::Rust),
            "use super::*"
        );
        assert_eq!(
            statement_of(&raw("crate::a::B", false), Lang::Rust),
            "use crate::a::B"
        );
        assert_eq!(statement_of(&raw("model", true), Lang::Rust), "mod model");
        assert_eq!(
            statement_of(&raw(".:views", false), Lang::Python),
            "from . import views"
        );
        assert_eq!(
            statement_of(&raw("..:up", false), Lang::Python),
            "from .. import up"
        );
        assert_eq!(
            statement_of(&raw("os.path", false), Lang::Python),
            "os.path"
        );
        assert_eq!(
            statement_of(&raw("./util", false), Lang::TypeScript),
            "./util"
        );
    }

    fn modules(src: &str, lang: &str) -> Vec<String> {
        imports_of(src, lang)
            .into_iter()
            .map(|i| i.module)
            .collect()
    }

    /// A Lean `import` names one module, its dotted name whole, whatever
    /// comes before it (`public`, `meta`) or after `import` (`all`).
    #[test]
    fn a_lean_import_names_its_whole_module() {
        let src = "module\n\nimport Mathlib.Data.Nat.Basic\npublic import Std.Data.HashMap\nimport Init\n\ndef x := 1\n";
        assert_eq!(
            modules(src, "lean"),
            ["Mathlib.Data.Nat.Basic", "Std.Data.HashMap", "Init"]
        );
        let lines: Vec<usize> = imports_of(src, "lean").iter().map(|i| i.line).collect();
        assert_eq!(lines, [3, 4, 5]);
    }

    /// Every import query compiles, once, for exactly the languages that have
    /// an import model.
    #[test]
    fn import_queries_compile_for_every_language_with_an_import_model() {
        for &lang in Lang::ALL {
            match import_query(lang) {
                Ok(_) => {}
                Err(QueryError::Unsupported) => assert!(
                    matches!(
                        lang,
                        Lang::C
                            | Lang::Cpp
                            | Lang::Json
                            | Lang::Bash
                            | Lang::Yaml
                            | Lang::Toml
                            | Lang::Html
                            | Lang::Css
                            | Lang::Scss
                            | Lang::Less
                            | Lang::Vue
                            | Lang::Svelte
                            | Lang::Astro
                            | Lang::Sql
                    ),
                    "{lang:?} lost its import model"
                ),
                Err(e) => panic!("{lang:?}: {e}"),
            }
        }
        // Compiled once: the same query object comes back.
        let a = import_query(Lang::Rust).unwrap() as *const Query;
        let b = import_query(Lang::Rust).unwrap() as *const Query;
        assert_eq!(a, b);
    }

    /// A grouped `use` names every path in it. Collapsing it to the common
    /// prefix sent `use crate::{a::b, c}` to the crate root.
    #[test]
    fn a_grouped_rust_use_yields_every_path_it_names() {
        let src = "use crate::{a::b, c, d::{self, e as f}, g::*};\n\
                   use std::collections::HashMap;\n\
                   use ::std::fs;\n\
                   use super::gamma::{\n    x,\n    y,\n};\n\
                   use self::inner::*;\n\
                   mod alpha;\nmod inline { use crate::z; }\n";
        let imports = imports_of(src, "rust");
        let uses: Vec<(&str, usize)> = imports
            .iter()
            .filter(|i| !i.is_mod_decl)
            .map(|i| (i.module.as_str(), i.line))
            .collect();
        assert_eq!(
            uses,
            vec![
                ("crate::a::b", 1),
                ("crate::c", 1),
                ("crate::d", 1),
                ("crate::d::e", 1),
                ("crate::g::*", 1),
                ("std::collections::HashMap", 2),
                ("::std::fs", 3),
                ("super::gamma::x", 5),
                ("super::gamma::y", 6),
                ("self::inner::*", 8),
                ("crate::z", 10),
            ]
        );
        let mods: Vec<&str> = imports
            .iter()
            .filter(|i| i.is_mod_decl)
            .map(|i| i.module.as_str())
            .collect();
        assert_eq!(mods, vec!["alpha"], "an inline `mod x {{}}` is not a file");
    }

    /// A `use` records whether it re-exports and whether it is a glob in the
    /// specifier itself, so the index cache and the wire carry both unchanged
    /// (the import graph needs them to tell a facade from a dependency).
    /// A module declaration carries the file its `#[path]` names, past its
    /// other attributes and comments; one under `cfg_attr` does not.
    #[test]
    fn a_rust_module_declaration_keeps_its_path_attribute() {
        let src = "#[path = \"sys/my win.rs\"]\n#[cfg(windows)]\n// The backend.\nmod windows;\nmod plain;\n#[cfg_attr(unix, path = \"u.rs\")]\nmod maybe;\n";
        let got = imports_of(src, "rust");
        let modules: Vec<&str> = got.iter().map(|r| r.module.as_str()).collect();
        assert_eq!(modules, ["windows=sys/my win.rs", "plain", "maybe"]);
        assert_eq!(
            statement_of(&got[0], Lang::Rust),
            "#[path = \"sys/my win.rs\"] mod windows"
        );
        assert_eq!(statement_of(&got[1], Lang::Rust), "mod plain");
    }

    #[test]
    fn rust_uses_record_reexports_and_globs() {
        let src = "pub use a::b;\npub(crate) use c::*;\npub(self) use d::e;\n\
                   use f::{g, h::*};\npub(in crate::x) use i::{j, k as l};\n\
                   #[cfg(test)]\npub(super) use m;\nmod n;\n";
        assert_eq!(
            modules(src, "rust"),
            [
                "pub a::b", "pub c::*", "d::e", "f::g", "f::h::*", "pub i::j", "pub i::k", "pub m",
                "n"
            ]
        );
        let u = crate::rustscope::RustUse::parse("pub c::*");
        assert!(u.glob && u.reexport && u.path == "c");
    }

    /// Deeply nested groups must not overflow the stack: the nesting is the
    /// repository's.
    #[test]
    fn deeply_nested_use_groups_do_not_recurse() {
        const DEPTH: usize = 5_000;
        let src = format!("use {}x{};\n", "a::{".repeat(DEPTH), "}".repeat(DEPTH));
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                // A path that long is no real one: it is left out.
                assert!(imports_of(&src, "rust").is_empty());
            })
            .unwrap()
            .join()
            .expect("expanding the use tree must not overflow the stack");
        // One nested within the length cap is kept whole.
        let src = format!("use {}x{};\n", "a::{".repeat(100), "}".repeat(100));
        let imports = imports_of(&src, "rust");
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].module.matches("::").count(), 100);
    }

    /// A list's entries share its prefix: a long prefix over many entries
    /// costs neither entries × prefix of memory nor more than the per-file
    /// cap of paths.
    #[test]
    fn a_wide_use_list_is_bounded() {
        let prefix = "a".repeat(1000);
        let src = format!("use {prefix}::{{{}}};\n", vec!["b"; 100_000].join(","));
        let imports = imports_of(&src, "rust");
        assert_eq!(imports.len(), MAX_RUST_IMPORTS);
        assert!(imports.iter().all(|i| i.module == format!("{prefix}::b")));
        // A prefix over the length cap yields nothing.
        let src = format!("use {}::{{b, c}};\n", "a".repeat(2000));
        assert!(imports_of(&src, "rust").is_empty());
        // Declarations count too: scoping copies their modules into each.
        let src = "mod m;\n".repeat(MAX_RUST_IMPORTS + 10);
        assert_eq!(imports_of(&src, "rust").len(), MAX_RUST_IMPORTS);
    }

    /// Python's relative form gives each name the dots it imports from, so
    /// it is bounded as a Rust list is: no more than the per-file cap of
    /// paths, and none over the length cap.
    #[test]
    fn a_wide_relative_python_import_is_bounded() {
        let names = vec!["b"; 100_000].join(", ");
        let src = format!("from {} import {names}\n", ".".repeat(1000));
        let imports = imports_of(&src, "python");
        assert_eq!(imports.len(), MAX_RUST_IMPORTS);
        assert!(imports.iter().all(|i| i.module.len() < MAX_RUST_IMPORT_LEN));
        let src = format!("from {} import b, c\n", ".".repeat(2000));
        assert!(imports_of(&src, "python").is_empty());
    }

    /// `from . import views` names `views` OF the package (a submodule or an
    /// attribute of its `__init__.py`), not the package itself; so does
    /// `from .pkg import x`, of `.pkg`, and `from mod import y`, of `mod`. A
    /// star import names the module alone.
    #[test]
    fn python_relative_from_import_names_each_submodule() {
        let src = "import os\nimport a.b.c as x\nfrom . import views, models as m\n\
                   from .. import (up)\nfrom . import *\nfrom .pkg import thing\nfrom mod import y\n\
                   from mod import *\n";
        assert_eq!(
            modules(src, "python"),
            vec![
                "os",
                "a.b.c",
                ".:views",
                ".:models",
                "..:up",
                ".",
                ".pkg:thing",
                "mod:y",
                "mod"
            ]
        );
    }

    /// CommonJS and dynamic imports are edges too; a computed specifier and a
    /// method called `require` are not.
    #[test]
    fn js_require_and_dynamic_import_are_extracted() {
        let src = "import a from './a';\nexport { b } from '../b';\n\
                   const c = require('./c');\nconst lazy = import('./lazy');\n\
                   require(name);\nloader.require('./not-an-import');\nimport 'side-effect';\n";
        for lang in ["javascript", "typescript", "tsx"] {
            assert_eq!(
                modules(src, lang),
                vec!["./a", "../b", "./c", "./lazy", "side-effect"],
                "{lang}"
            );
        }
        let ts = "import fs = require('fs');\nimport type { T } from './types';\n";
        assert_eq!(modules(ts, "typescript"), vec!["fs", "./types"]);
    }

    #[test]
    fn go_imports_in_every_form() {
        let src = "package p\n\nimport (\n\t\"fmt\"\n\tx \"example.com/a/b\"\n\t. \"example.com/c\"\n\t_ \"example.com/d\"\n)\n\
                   import \"os\"\nimport `raw/path`\n";
        let imports = imports_of(src, "go");
        let got: Vec<(&str, usize)> = imports
            .iter()
            .map(|i| (i.module.as_str(), i.line))
            .collect();
        assert_eq!(
            got,
            vec![
                ("fmt", 4),
                ("example.com/a/b", 5),
                ("example.com/c", 6),
                ("example.com/d", 7),
                ("os", 9),
                ("raw/path", 10),
            ]
        );
    }

    #[test]
    fn dart_imports_and_exports() {
        let src = "import 'dart:async';\nimport 'src/p.dart' as p;\nexport 'src/q.dart' show Q;\n";
        assert_eq!(
            modules(src, "dart"),
            vec!["dart:async", "src/p.dart", "src/q.dart"]
        );
    }

    #[test]
    fn languages_without_an_import_model_extract_nothing() {
        assert!(imports_of("{\"a\": 1}\n", "json").is_empty());
        assert!(imports_of("source ./lib.sh\n", "bash").is_empty());
        assert!(imports_of("x", "klingon").is_empty());
    }

    /// The module of each import, and whether it is a declaration.
    fn entries(src: &str, lang: &str) -> Vec<(String, bool)> {
        imports_of(src, lang)
            .into_iter()
            .map(|i| (i.module, i.is_mod_decl))
            .collect()
    }

    fn uses(modules: &[&str]) -> Vec<(String, bool)> {
        modules.iter().map(|m| (m.to_string(), false)).collect()
    }

    /// A Java file declares its package (no import: what its own imports
    /// resolve under) and imports a class, a package's every class, or a
    /// class's static member.
    #[test]
    fn java_imports_and_the_package_they_resolve_from() {
        let src = "@Deprecated\npackage com.acme.app;\n\nimport com.acme.util.Strings;\n\
                   import java.util.*;\nimport static org.junit.Assert.assertEquals;\n\
                   import static com.acme.Consts.*;\nclass A {}\n";
        let mut want = vec![("com.acme.app".to_string(), true)];
        want.extend(uses(&[
            "com.acme.util.Strings",
            "java.util.*",
            "org.junit.Assert.assertEquals",
            "com.acme.Consts.*",
        ]));
        assert_eq!(entries(src, "java"), want);
        let lines: Vec<usize> = imports_of(src, "java").iter().map(|i| i.line).collect();
        assert_eq!(lines, [2, 4, 5, 6, 7]);
    }

    /// Kotlin's imports are Java's — a star, an alias the same import —
    /// and so is its package; Scala's chained packages are one, a selector
    /// group names each of its members, a wildcard (`_` or `*`) the
    /// package; Elixir's `alias`, `import`, `use` and `require` each name
    /// a module (`alias A.{B, C}` two), and a file declares each module it
    /// defines, a nested one by its full name.
    #[test]
    fn kotlin_scala_and_elixir_imports() {
        let kt = "package com.acme.shop\n\nimport kotlin.math.max\nimport com.acme.model.*\n\
                  import com.acme.repo.OrderRepository as Repo\n\nclass A\n";
        let mut want = vec![("com.acme.shop".to_string(), true)];
        want.extend(uses(&[
            "kotlin.math.max",
            "com.acme.model.*",
            "com.acme.repo.OrderRepository",
        ]));
        assert_eq!(entries(kt, "kotlin"), want);
        let scala = "package com.acme\npackage shop\n\nimport scala.util.{Try, Success => Ok}\n\
                     import com.acme.model._\nimport com.acme.repo.OrderRepository\n\
                     import cats.implicits.*\n\nclass A\n";
        let mut want = uses(&[
            "scala.util.Try",
            "scala.util.Success",
            "com.acme.model.*",
            "com.acme.repo.OrderRepository",
            "cats.implicits.*",
        ]);
        want.push(("com.acme.shop".to_string(), true));
        assert_eq!(entries(scala, "scala"), want);
        let ex = "defmodule Shop.Checkout do\n  alias Shop.{Order, Repo}\n  alias Shop.Pricing.Tax\n\
                  import Ecto.Query, only: [from: 2]\n  use GenServer\n  require Logger\n\n\
                  defmodule Error do\n    defexception [:message]\n  end\nend\n";
        let mut got = entries(ex, "elixir");
        got.sort();
        let mut want = uses(&[
            "Shop.Order",
            "Shop.Repo",
            "Shop.Pricing.Tax",
            "Ecto.Query",
            "GenServer",
            "Logger",
        ]);
        want.push(("Shop.Checkout".to_string(), true));
        want.push(("Shop.Checkout.Error".to_string(), true));
        want.sort();
        assert_eq!(got, want);
    }

    /// A C# `using` names a namespace or a type, however it is written —
    /// `global`, `static`, aliased, with type arguments — and a file declares
    /// every namespace it opens, a nested one by its full name.
    #[test]
    fn csharp_usings_and_the_namespaces_a_file_declares() {
        let src = "global using Acme.Core;\nusing static Acme.Util.Strings;\n\
                   using Map = System.Collections.Generic.Dictionary<string, int>;\n\
                   using global::System.Text;\nusing System;\n\
                   namespace Acme.App {\n  using Acme.Data;\n  namespace Web { class C {} }\n}\n\
                   namespace Acme.Other { }\n";
        let mut want = uses(&[
            "Acme.Core",
            "Acme.Util.Strings",
            "System.Collections.Generic.Dictionary",
            "System.Text",
            "System",
            "Acme.Data",
        ]);
        want.extend([
            ("Acme.App".to_string(), true),
            ("Acme.App.Web".to_string(), true),
            ("Acme.Other".to_string(), true),
        ]);
        assert_eq!(entries(src, "csharp"), want);
        // A file-scoped namespace declares the file's one namespace.
        assert_eq!(
            entries(
                "namespace Acme.Lib;\nusing Acme.Core;\nclass X {}\n",
                "csharp"
            ),
            [
                ("Acme.Core".to_string(), false),
                ("Acme.Lib".to_string(), true)
            ]
        );
    }

    /// A PHP `use` names each class (function, constant) it brings in, a
    /// group's under its prefix; `require`/`include` name the path a literal
    /// writes down, also when it is appended to the file's own directory —
    /// not one computed any other way.
    #[test]
    fn php_uses_and_required_paths() {
        let src = "<?php\nnamespace App\\Http;\nuse App\\Models\\User;\n\
                   use App\\Support\\{Str, Arr as A};\nuse function App\\Helpers\\format_date;\n\
                   use \\Psr\\Log\\LoggerInterface as Log, Monolog\\Logger;\n\
                   class K { use SomeTrait; }\n\
                   require 'config.php';\nrequire_once __DIR__ . '/lib/a.php';\n\
                   include(\"views/b.php\");\ninclude_once dirname(__FILE__) . '/c.php';\n\
                   require dirname(__DIR__) . '/vendor/autoload.php';\n\
                   require $path;\ninclude \"x$y.php\";\n";
        assert_eq!(
            modules(src, "php"),
            [
                "App\\Models\\User",
                "App\\Support\\Str",
                "App\\Support\\Arr",
                "App\\Helpers\\format_date",
                "Psr\\Log\\LoggerInterface",
                "Monolog\\Logger",
                "config.php",
                "__DIR__/lib/a.php",
                "views/b.php",
                "__DIR__/c.php",
                "__DIR__/../vendor/autoload.php",
            ]
        );
    }

    /// Ruby's `require` and `load` name a feature on the load path, a
    /// `require_relative` a path relative to the file; only a plain string
    /// is an import, and only as a call of the file's own (no receiver).
    #[test]
    fn ruby_requires_of_a_literal() {
        let src = "require \"json\"\nrequire 'acme/widget' unless defined?(Acme)\n\
                   require_relative 'support/helper'\nrequire_relative \"../lib/x\"\n\
                   load \"tasks/setup.rb\"\nrequire(\"set\")\nrequire \"a#{b}\"\n\
                   Kernel.require 'not_this'\nrequire name\n";
        assert_eq!(
            modules(src, "ruby"),
            [
                "json",
                "acme/widget",
                "./support/helper",
                "../lib/x",
                "tasks/setup.rb",
                "set"
            ]
        );
    }

    /// Erlang headers by path (an `-include_lib`'s marked as such) and the
    /// modules a file implements or imports from; a remote call names no
    /// import.
    #[test]
    fn erlang_includes_behaviours_and_imports() {
        let src = "-module(m).\n-include(\"m.hrl\").\n-include_lib(\"kernel/include/file.hrl\").\n\
                   -include(\"inc/\" \"defs.hrl\").\n-include(?DIR \"x.hrl\").\n\
                   -behaviour(gen_server).\n-behavior('my_behaviour').\n\
                   -import(lists, [map/2]).\nf() -> io:format(\"x\").\n";
        assert_eq!(
            modules(src, "erlang"),
            [
                "m.hrl",
                "lib:kernel/include/file.hrl",
                "inc/defs.hrl",
                "gen_server",
                "my_behaviour",
                "lists"
            ]
        );
        let lib = &imports_of(src, "erlang")[1];
        assert_eq!(
            statement_of(lib, Lang::Erlang),
            "-include_lib(\"kernel/include/file.hrl\")"
        );
    }

    /// A Haskell import names its module, whatever qualifies it; an alias is
    /// no import.
    #[test]
    fn haskell_imports_name_their_module() {
        let src = "module Main where\nimport qualified Data.Map as M\nimport Foo.Bar (x)\n\
                   import \"text\" Data.Text\nimport Data.List hiding (sort)\nmain = pure ()\n";
        assert_eq!(
            modules(src, "haskell"),
            ["Data.Map", "Foo.Bar", "Data.Text", "Data.List"]
        );
    }

    /// Zig's `@import` names a file or a package; no other builtin imports.
    /// C's and C++'s includes: a quoted one by its path, an angle one with
    /// its brackets; an include of a macro, and one in a comment, by none.
    #[test]
    fn c_and_cpp_includes() {
        let src = "#include <stdio.h>\n#include \"util/x.h\"\n#include CONFIG_H\n\
                   // #include \"no.h\"\n#if 1\n#  include <sys/stat.h>\n#endif\n";
        assert_eq!(modules(src, "c"), ["<stdio.h>", "util/x.h", "<sys/stat.h>"]);
        let cpp = "#include <vector>\n#include \"cart.hpp\"\nint main() {}\n";
        assert_eq!(modules(cpp, "cpp"), ["<vector>", "cart.hpp"]);
    }

    #[test]
    fn zig_imports_files_and_packages() {
        let src = "const std = @import(\"std\");\nconst x = @import(\"util/x.zig\");\n\
                   const F = @import(\"../f.zig\").Foo;\nconst e = @embedFile(\"a.txt\");\n";
        assert_eq!(modules(src, "zig"), ["std", "util/x.zig", "../f.zig"]);
    }

    /// A PHP group gives each entry its prefix, and a nested C# namespace its
    /// enclosing ones' names: both are bounded as a Rust group is — no more
    /// than the per-file cap of entries, none over the length cap.
    #[test]
    fn wide_php_groups_and_deep_csharp_namespaces_are_bounded() {
        let prefix = vec!["A"; 400].join("\\");
        let src = format!(
            "<?php\nuse {prefix}\\{{{}}};\n",
            vec!["B"; 100_000].join(", ")
        );
        let imports = imports_of(&src, "php");
        assert_eq!(imports.len(), MAX_RUST_IMPORTS);
        assert!(imports.iter().all(|i| i.module == format!("{prefix}\\B")));
        let long = format!("<?php\nuse {}\\{{B, C}};\n", "A".repeat(2000));
        assert!(imports_of(&long, "php").is_empty());
        // Each level adds 11 bytes (`.Abcdefghij`): 93 levels fit the cap,
        // and the 7 nested deeper are left out.
        let depth = 100;
        let src = format!(
            "{}class C {{}}{}",
            "namespace Abcdefghij {".repeat(depth),
            "}".repeat(depth)
        );
        let spaces = imports_of(&src, "csharp");
        assert_eq!(spaces.len(), 93);
        assert!(
            spaces
                .iter()
                .all(|i| i.is_mod_decl && i.module.len() <= MAX_RUST_IMPORT_LEN)
        );
    }

    /// The model reads a Java or C# declaration as one, not as an import.
    #[test]
    fn a_declaration_reads_as_one() {
        let decl = |module: &str| RawImport {
            module: module.into(),
            line: 1,
            is_mod_decl: true,
        };
        assert_eq!(statement_of(&decl("a.b"), Lang::Java), "package a.b");
        assert_eq!(statement_of(&decl("A.B"), Lang::CSharp), "namespace A.B");
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    /// A build.zig's module names, as each Zig writes them: a module bound
    /// to a name and given in `.imports`, by `addImport`, `b.addModule`,
    /// inline, anonymously, Zig 0.11's `.dependencies` and `addModule`; a
    /// dependency's or a generated module, a path out of the project, a
    /// commented-out line: none.
    #[test]
    fn a_build_zig_names_its_modules_and_their_files() {
        let build = r#"
const std = @import("std");
// const old = b.createModule(.{ .root_source_file = b.path("src/old.zig") });

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const greet = b.createModule(.{
        .root_source_file = b.path("src/greet.zig"),
        .target = target,
    });
    const util: *std.Build.Module = b.addModule("shop-util", .{ .root_source_file = b.path("./lib/util.zig") });
    const zap = b.dependency("zap", .{});
    const options = b.addOptions();
    const exe = b.addExecutable(.{
        .name = "shop",
        .root_module = b.createModule(.{
            .root_source_file = b.path("src/main.zig"),
            .target = target,
            .imports = &.{
                .{ .name = "greet", .module = greet },
                .{ .name = "util", .module = util },
                .{ .name = "zap", .module = zap.module("zap") },
                .{ .name = "inline", .module = b.createModule(.{ .root_source_file = b.path("src/inline.zig") }) },
            },
        }),
    });
    exe.root_module.addImport("strings", b.createModule(.{ .root_source_file = b.path("src/strings.zig") }));
    exe.root_module.addImport("again", greet);
    exe.root_module.addOptions("config", options);
    exe.root_module.addAnonymousImport("assets", .{ .root_source_file = b.path("assets/index.zig") });
    exe.root_module.addImport("outside", b.createModule(.{ .root_source_file = b.path("../x.zig") }));
    const sep = '"';
    _ = sep;
    // Zig 0.11
    const old = b.createModule(.{ .source_file = .{ .path = "src/legacy.zig" } });
    exe.addModule("legacy", old);
    const lib = b.addStaticLibrary(.{ .name = "x", .root_source_file = .{ .path = "src/lib.zig" } });
    _ = lib;
    const dep_mod = b.createModule(.{ .source_file = .{ .path = "src/dep.zig" }, .dependencies = &.{ .{ .name = "legacy2", .module = old } } });
    _ = dep_mod;
}
"#;
        let got = zig_modules(build);
        let want: Vec<(String, String)> = [
            ("again", "src/greet.zig"),
            ("assets", "assets/index.zig"),
            ("greet", "src/greet.zig"),
            ("inline", "src/inline.zig"),
            ("legacy", "src/legacy.zig"),
            ("legacy2", "src/legacy.zig"),
            ("shop-util", "lib/util.zig"),
            ("strings", "src/strings.zig"),
            ("util", "lib/util.zig"),
        ]
        .iter()
        .map(|(n, f)| (n.to_string(), f.to_string()))
        .collect();
        assert_eq!(got, want);
        assert!(bears_on_manifests("pkg/build.zig"));
        assert!(!bears_on_manifests("src/main.zig"));
    }

    /// The manifests are read only for a project in their language —
    /// `package.yaml` is pnpm's too — the shallowest first, and
    /// [`bears_on_manifests`] says which changes ask for them again.
    #[test]
    fn manifests_are_read_for_their_language_shallowest_first() {
        let dir = dir("source-manifests");
        std::fs::create_dir_all(dir.join("pkgs/a")).unwrap();
        std::fs::write(dir.join("package.yaml"), "source-dirs: src\n").unwrap();
        std::fs::write(
            dir.join("pkgs/a/a.cabal"),
            "library\n  hs-source-dirs: lib\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("composer.json"),
            r#"{"autoload":{"psr-4":{"App\\":"app/"}}}"#,
        )
        .unwrap();
        let without = [
            "package.yaml",
            "pkgs/a/a.cabal",
            "composer.json",
            "web/app.ts",
        ];
        assert!(source_manifests(&dir, &without).is_empty());
        let with = [
            "pkgs/a/a.cabal",
            "package.yaml",
            "composer.json",
            "app/A.php",
            "Main.hs",
        ];
        let read = source_manifests(&dir, &with);
        assert_eq!(read.psr4[0].0, "composer.json");
        assert_eq!(
            read.haskell,
            [
                ("package.yaml".to_string(), vec!["src".to_string()]),
                ("pkgs/a/a.cabal".to_string(), vec!["lib".to_string()])
            ]
        );
        for rel in [
            "composer.json",
            "x/pkg.cabal",
            "package.yaml",
            "a/B.php",
            "M.hs",
            "L.lhs",
        ] {
            assert!(bears_on_manifests(rel), "{rel}");
        }
        assert!(bears_on_manifests("packages/api/package.json"));
        for rel in ["src/lib.rs", ".cabal", "src/app.ts"] {
            assert!(!bears_on_manifests(rel), "{rel}");
        }
    }

    /// A `package.json`'s name and exports — a subpath map, conditions,
    /// one file — its sources tried before its build, and `main` and
    /// `types` where `exports` says nothing of the package itself.
    #[test]
    fn a_js_package_says_its_name_and_exports() {
        let p = js_package(
            r#"{"name":"@acme/api","exports":{".":{"types":"./dist/index.d.ts","default":"./src/index.ts"},"./client":"./src/client.ts","./*":"./src/*.ts"}}"#,
        )
        .unwrap();
        assert_eq!(p.name, "@acme/api");
        assert_eq!(
            p.exports,
            [
                (
                    ".".to_string(),
                    vec![
                        "./src/index.ts".to_string(),
                        "./dist/index.d.ts".to_string()
                    ]
                ),
                ("./*".to_string(), vec!["./src/*.ts".to_string()]),
                ("./client".to_string(), vec!["./src/client.ts".to_string()]),
            ]
        );
        let p = js_package(r#"{"name":"ui","main":"dist/index.js","types":"dist/index.d.ts","source":"src/index.tsx"}"#)
            .unwrap();
        assert_eq!(
            p.exports,
            [(
                ".".to_string(),
                vec![
                    "src/index.tsx".to_string(),
                    "dist/index.js".to_string(),
                    "dist/index.d.ts".to_string()
                ]
            )]
        );
        assert_eq!(
            js_package(r#"{"name":"x","exports":"./index.js"}"#)
                .unwrap()
                .exports[0]
                .1,
            ["./index.js"]
        );
        assert!(
            js_package(r#"{"private":true}"#).is_none(),
            "no name: none can import it"
        );
        assert!(js_package("not json").is_none());
        assert!(
            js_package(r#"{"name":"x","main":"../../etc/passwd"}"#)
                .unwrap()
                .exports
                .is_empty(),
            "nothing outside its folder"
        );
    }

    fn dir(name: &str) -> crate::testutil::TempDir {
        crate::testutil::TempDir::new(name)
    }

    /// A Cargo manifest says what its package's library is called in code
    /// (`[lib] name`, else the package's name with `-` as `_`), where it is,
    /// and where its other targets are; a workspace's own manifest has no
    /// package; a path that leaves the package is not taken.
    #[test]
    fn a_cargo_manifest_names_its_library_and_targets() {
        let plain = cargo_package("[package]\nname = \"my-core\"\nversion = \"0.1.0\"\n").unwrap();
        assert_eq!(plain.lib_name, "my_core");
        assert_eq!(plain.lib_path, "src/lib.rs");
        assert!(plain.target_paths.is_empty());
        let own = cargo_package(
            "[package]\nname = \"wire\"\n\n[lib]\nname = \"wire_proto\"\npath = \"lib/wire.rs\"\n\n\
             [[bin]]\nname = \"gen\"\npath = \"tools/gen.rs\"\n\n[[bin]]\nname = \"plain\"\n\n\
             [[example]]\nname = \"demo\"\npath = \"../outside/demo.rs\"\n\n\
             [[test]]\nname = \"it\"\npath = \"checks/it.rs\"\n",
        )
        .unwrap();
        assert_eq!(own.lib_name, "wire_proto");
        assert_eq!(own.lib_path, "lib/wire.rs");
        assert_eq!(own.target_paths, ["tools/gen.rs", "checks/it.rs"]);
        let escaping =
            cargo_package("[package]\nname = \"x\"\n[lib]\npath = \"/etc/lib.rs\"\n").unwrap();
        assert_eq!(escaping.lib_path, "src/lib.rs");
        assert_eq!(
            cargo_package("[workspace]\nmembers = [\"crates/*\"]\n"),
            None
        );
        assert_eq!(cargo_package("not toml ["), None);
    }

    /// Each tool's way of saying where a project's Python lives is read, in
    /// the order written, each root once; the project's own folder and a
    /// path out of it are left out.
    #[test]
    fn a_pyproject_names_its_python_source_roots() {
        let roots = |text: &str| python_source_roots(text);
        assert_eq!(
            roots("[tool.setuptools.packages.find]\nwhere = [\"src\"]\n"),
            ["src"]
        );
        assert_eq!(
            roots("[tool.setuptools.package-dir]\n\"\" = \"lib\"\n"),
            ["lib"]
        );
        assert_eq!(
            roots(
                "[tool.poetry]\npackages = [{ include = \"acme\", from = \"src\" }, \
                 { include = \"extra\" }]\n"
            ),
            ["src"]
        );
        assert_eq!(
            roots("[tool.hatch.build.targets.wheel]\npackages = [\"src/acme\", \"tools/\"]\n"),
            ["src"]
        );
        assert_eq!(
            roots("[tool.maturin]\npython-source = \"python\"\n"),
            ["python"]
        );
        assert_eq!(
            roots("[tool.pdm.build]\npackage-dir = \"pkgs\"\n"),
            ["pkgs"]
        );
        assert_eq!(
            roots("[tool.pytest.ini_options]\npythonpath = \"src tests/helpers\"\n"),
            ["src", "tests/helpers"]
        );
        assert_eq!(
            roots(
                "[tool.pytest.ini_options]\npythonpath = [\".\", \"./src/\"]\n\
                 [tool.pyright]\nextraPaths = [\"src\", \"vendor\", \"../shared\"]\n\
                 [tool.mypy]\nmypy_path = \"stubs:$MYPY_CONFIG_FILE_DIR/x, extra\"\n"
            ),
            ["src", "vendor", "stubs", "extra"]
        );
        assert!(roots("[project]\nname = \"plain\"\n").is_empty());
        assert!(roots("not toml [").is_empty());
    }

    /// The Cargo and pyproject manifests are read with the others, and their
    /// changes ask for them again.
    #[test]
    fn cargo_and_pyproject_manifests_are_read_with_the_others() {
        let dir = dir("cargo-pyproject-manifests");
        std::fs::create_dir_all(dir.join("crates/core")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("crates/core/Cargo.toml"),
            "[package]\nname = \"acme-core\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("pyproject.toml"),
            "[tool.setuptools.packages.find]\nwhere = [\"src\"]\n",
        )
        .unwrap();
        let read = source_manifests(
            &dir,
            &["crates/core/Cargo.toml", "Cargo.toml", "pyproject.toml"],
        );
        assert_eq!(read.cargo.len(), 1, "the workspace's own has no package");
        assert_eq!(read.cargo[0].0, "crates/core/Cargo.toml");
        assert_eq!(read.cargo[0].1.lib_name, "acme_core");
        assert_eq!(
            read.python,
            [("pyproject.toml".to_string(), vec!["src".to_string()])]
        );
        for rel in ["Cargo.toml", "crates/x/Cargo.toml", "pyproject.toml"] {
            assert!(bears_on_manifests(rel), "{rel}");
        }
    }

    /// `go.mod` and `pubspec.yaml` ship with the repository, so their type and
    /// size are attacker-chosen — and both are read automatically on every
    /// project open. A link to an endless device must read as "absent", not
    /// hang the open.
    #[test]
    fn project_metadata_reads_are_bounded_and_plain_file_only() {
        let d = dir("imports-metadata");
        std::fs::write(d.join("go.mod"), "module example.com/m\n").unwrap();
        assert_eq!(read_go_module(&d).as_deref(), Some("example.com/m"));
        std::fs::write(d.join("pubspec.yaml"), "name: demo\n").unwrap();
        assert_eq!(read_dart_package(&d).as_deref(), Some("demo"));

        #[cfg(unix)]
        {
            let evil = dir("imports-metadata-evil");
            std::os::unix::fs::symlink("/dev/zero", evil.join("go.mod")).unwrap();
            assert!(
                read_go_module(&evil).is_none(),
                "a go.mod pointing at an endless device must not be read"
            );
            std::os::unix::fs::symlink("/dev/zero", evil.join("pubspec.yaml")).unwrap();
            assert!(read_dart_package(&evil).is_none());
        }

        // Over the cap: refused rather than pulled into memory.
        let big = dir("imports-metadata-big");
        let mut text = String::from("module example.com/m\n");
        text.push_str(&"# pad\n".repeat(MAX_METADATA_BYTES as usize / 6 + 16));
        std::fs::write(big.join("go.mod"), &text).unwrap();
        assert!(read_go_module(&big).is_none());
    }

    /// composer.json's PSR-4 map (both sections, one directory or several),
    /// a Cabal file's `hs-source-dirs` (every stanza's, continued over lines)
    /// and package.yaml's `source-dirs` (every form YAML writes a list in) —
    /// each read through the same bounded, plain-file-only reader.
    #[test]
    fn php_and_haskell_metadata_reads_are_bounded_and_plain_file_only() {
        let d = dir("imports-metadata-langs");
        let composer = d.join("composer.json");
        std::fs::write(
            &composer,
            r#"{"name": "acme/app",
                "autoload": {"psr-4": {"App\\": "app/", "Acme\\Lib\\": ["src/", "lib/"]}},
                "autoload-dev": {"psr-4": {"Tests\\": "tests/", "": "fallback/"}}}"#,
        )
        .unwrap();
        assert_eq!(
            read_composer_psr4(&composer),
            [
                (
                    "Acme\\Lib\\".to_string(),
                    vec!["src/".to_string(), "lib/".to_string()]
                ),
                ("App\\".to_string(), vec!["app/".to_string()]),
                (String::new(), vec!["fallback/".to_string()]),
                ("Tests\\".to_string(), vec!["tests/".to_string()]),
            ]
        );
        let cabal = d.join("acme.cabal");
        std::fs::write(
            &cabal,
            "name: acme\nlibrary\n  exposed-modules: Acme\n  hs-source-dirs:   src\n\
             \x20                 , gen\n  -- a comment\n\
             executable acme\n  main-is: Main.hs\n  Hs-Source-Dirs: app\n",
        )
        .unwrap();
        assert_eq!(read_cabal_source_dirs(&cabal), ["src", "gen", "app"]);
        let hpack = d.join("package.yaml");
        std::fs::write(
            &hpack,
            "name: acme\nlibrary:\n  source-dirs: src # the library\n\
             executables:\n  acme:\n    source-dirs: [app, \"cli\"]\n\
             tests:\n  spec:\n    source-dirs:\n    - test\n    - 'test-util'\n    main: Spec.hs\n",
        )
        .unwrap();
        assert_eq!(
            read_hpack_source_dirs(&hpack),
            ["src", "app", "cli", "test", "test-util"]
        );

        #[cfg(unix)]
        {
            let evil = dir("imports-metadata-langs-evil");
            for name in ["composer.json", "acme.cabal", "package.yaml"] {
                std::os::unix::fs::symlink("/dev/zero", evil.join(name)).unwrap();
            }
            assert!(read_composer_psr4(&evil.join("composer.json")).is_empty());
            assert!(read_cabal_source_dirs(&evil.join("acme.cabal")).is_empty());
            assert!(read_hpack_source_dirs(&evil.join("package.yaml")).is_empty());
        }

        let big = dir("imports-metadata-langs-big");
        let mut text = String::from("library\n  hs-source-dirs: src\n");
        text.push_str(&"-- pad\n".repeat(MAX_METADATA_BYTES as usize / 7 + 16));
        std::fs::write(big.join("big.cabal"), &text).unwrap();
        assert!(read_cabal_source_dirs(&big.join("big.cabal")).is_empty());
    }
}

/// Whether `lang` is a page's or a stylesheet's, whose imports are read off
/// the tree without a query: HTML's scripts and stylesheets, CSS's, SCSS's
/// and Less's `@import`s.
fn is_web_page_or_style(lang: Lang) -> bool {
    matches!(lang, Lang::Html | Lang::Css | Lang::Scss | Lang::Less)
}

/// What a page loads as code: a `<script src>`, and a `<link>` to a
/// stylesheet or a preloaded module or style (`rel="stylesheet"`,
/// `modulepreload`, `preload` as a script or style) — Vite's `index.html`
/// and its `/src/main.ts`. Each URL as written; the resolver tells the
/// project's files from the web's.
fn html_imports(root: Node, source: &str) -> Vec<RawImport> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
        if !matches!(node.kind(), "start_tag" | "self_closing_tag") {
            continue;
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.named_children(&mut cursor).collect();
        let Some(tag) = children
            .iter()
            .find(|c| c.kind() == "tag_name")
            .and_then(|t| source.get(t.byte_range()))
        else {
            continue;
        };
        let attr = |name: &str| -> Option<String> {
            children
                .iter()
                .filter(|c| c.kind() == "attribute")
                .find_map(|a| {
                    let mut cursor = a.walk();
                    let parts: Vec<Node> = a.named_children(&mut cursor).collect();
                    let key = parts.first().and_then(|k| source.get(k.byte_range()))?;
                    if !key.eq_ignore_ascii_case(name) {
                        return None;
                    }
                    let value = parts.get(1)?;
                    let value = match value.kind() {
                        "quoted_attribute_value" => value.named_child(0)?,
                        _ => *value,
                    };
                    source.get(value.byte_range()).map(|v| v.trim().to_string())
                })
        };
        let url = match tag.to_ascii_lowercase().as_str() {
            "script" => attr("src"),
            "link" => {
                let rel = attr("rel").unwrap_or_default().to_ascii_lowercase();
                let kind = attr("as").unwrap_or_default().to_ascii_lowercase();
                let loads = rel.split_whitespace().any(|r| {
                    matches!(r, "stylesheet" | "modulepreload")
                        || (r == "preload" && matches!(kind.as_str(), "script" | "style"))
                });
                loads.then(|| attr("href")).flatten()
            }
            _ => None,
        };
        if let Some(url) = url.filter(|u| !u.is_empty() && !u.starts_with("data:")) {
            out.push(RawImport {
                module: url,
                line: node.start_position().row + 1,
                is_mod_decl: false,
            });
        }
    }
    out.sort_by_key(|r| r.line);
    out
}

/// A stylesheet's imports: CSS's `@import "x.css"` and `@import url(x.css)`,
/// SCSS's `@use`, `@forward` and `@import "a", "b"`, Less's `@import
/// (reference) "x"`. A `@use … with (…)`'s values are no imports: a `@use`
/// or `@forward` names one, its first string.
fn style_imports(root: Node, source: &str) -> Vec<RawImport> {
    fn strings<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
        match node.kind() {
            "string_value" => out.push(node),
            // `url(x.css)`: its argument, quoted or not.
            "call_expression" => {
                if let Some(arguments) = node
                    .named_children(&mut node.walk())
                    .find(|c| c.kind() == "arguments")
                    && let Some(first) = arguments.named_child(0)
                {
                    out.push(first);
                }
            }
            // A `@use`'s `with (…)` and Less's `(reference)`: no paths.
            "parenthesized_value" => {}
            _ => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    strings(child, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if !matches!(
            kind,
            "import_statement" | "use_statement" | "forward_statement"
        ) {
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
            continue;
        }
        let mut found = Vec::new();
        strings(node, &mut found);
        if kind != "import_statement" {
            found.truncate(1);
        }
        for s in found {
            let text = source.get(s.byte_range()).unwrap_or("").trim();
            let path = text.trim_matches(['"', '\'']).trim();
            if !path.is_empty() {
                out.push(RawImport {
                    module: path.to_string(),
                    line: s.start_position().row + 1,
                    is_mod_decl: false,
                });
            }
        }
    }
    out.sort_by_key(|r| r.line);
    out
}

#[cfg(test)]
mod web_tests {
    use super::*;

    fn modules(src: &str, lang: &str) -> Vec<(String, usize)> {
        imports_of(src, lang)
            .into_iter()
            .map(|r| (r.module, r.line))
            .collect()
    }

    fn m(s: &str, line: usize) -> (String, usize) {
        (s.to_string(), line)
    }

    /// A stylesheet's `@import`, `@use` and `@forward`, a comment's
    /// aside, a `@use`'s `with (…)` values and Less's options aside.
    #[test]
    fn stylesheet_imports() {
        let css =
            "@import url(\"a.css\") screen;\n@import 'b.css';\n/* @import 'c.css'; */\n.x {}\n";
        assert_eq!(modules(css, "css"), [m("a.css", 1), m("b.css", 2)]);
        let scss = "@use 'sass:math';\n@use \"theme\" as t with ($a: \"x\");\n@forward 'src/list';\n@import 'a', 'b';\n// @import 'z';\n";
        assert_eq!(
            modules(scss, "scss"),
            [
                m("sass:math", 1),
                m("theme", 2),
                m("src/list", 3),
                m("a", 4),
                m("b", 4)
            ]
        );
        let less = "@import (reference) \"foo.less\";\n@import 'bar';\n";
        assert_eq!(modules(less, "less"), [m("foo.less", 1), m("bar", 2)]);
    }

    /// What a page loads: its scripts and stylesheets, a preloaded module;
    /// not its images, links or icons.
    #[test]
    fn a_pages_scripts_and_stylesheets() {
        let html = "<!doctype html>\n<html><head>\n<link rel=\"stylesheet\" href=\"/style.css\">\n<link rel=\"icon\" href=\"/favicon.ico\">\n<link rel=\"modulepreload\" href=\"./vendor.js\">\n</head><body>\n<img src=\"logo.png\">\n<script type=\"module\" src=\"/src/main.ts\"></script>\n<script>inline()</script>\n<script src=\"https://cdn.example.com/x.js\"></script>\n</body></html>\n";
        assert_eq!(
            modules(html, "html"),
            [
                m("/style.css", 3),
                m("./vendor.js", 5),
                m("/src/main.ts", 8),
                m("https://cdn.example.com/x.js", 10)
            ]
        );
    }
}
