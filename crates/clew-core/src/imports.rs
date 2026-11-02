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
    let Some(lang) = Lang::for_source(lang, source) else {
        return Vec::new();
    };
    // Fetch (or compile) the query first, so a language without an import
    // model is not parsed at all.
    if import_query(lang).is_err() {
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
    let Ok(query) = import_query(lang) else {
        return Vec::new();
    };
    let root = tree.root_node();
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
        Lang::C
        | Lang::Cpp
        | Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
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
        Lang::C
        | Lang::Cpp
        | Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
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
        || is_haskell_manifest(name)
        || [".php", ".hs", ".lhs"]
            .iter()
            .any(|ext| name.ends_with(ext))
}

/// What the project's package manifests, among its files `rels`, say of
/// where its sources are: each `composer.json`'s PSR-4 map when the project
/// has PHP, each Haskell package description's source directories when it
/// has Haskell — only then, as `package.yaml` is pnpm's file too. The
/// shallowest [`MAX_MANIFESTS`] of each kind, read bounded and
/// plain-file-only, where the files live.
pub fn source_manifests<S: AsRef<str>>(root: &Path, rels: &[S]) -> clew_protocol::SourceManifests {
    let rels: Vec<&str> = rels.iter().map(AsRef::as_ref).collect();
    let has = |exts: &[&str]| rels.iter().any(|r| exts.iter().any(|ext| r.ends_with(ext)));
    let picked = |is_manifest: &dyn Fn(&str) -> bool| {
        let mut found: Vec<&str> = rels
            .iter()
            .copied()
            .filter(|r| is_manifest(r.rsplit(['/', '\\']).next().unwrap_or(r)))
            .collect();
        found.sort_by_key(|r| (r.split(['/', '\\']).count(), *r));
        found.truncate(MAX_MANIFESTS);
        found
    };
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
    out
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
                emit(
                    RawImport {
                        module: node_text(name, src).to_string(),
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
        Lang::Rust if raw.is_mod_decl => format!("mod {}", raw.module),
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

/// Separates a relative package from a name imported out of it, for
/// `from . import views` (`.:views`) and `from .. import up` (`..:up`): the
/// name is the package's submodule when one exists, else an attribute of the
/// package's `__init__.py`. Never part of a module path.
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
        // (see [`PY_NAME_SEP`]) keeps both readings for the resolver. Only a
        // bare relative prefix needs this: in `from .pkg import x` the module
        // is `.pkg` itself.
        if !text.is_empty() && text.chars().all(|c| c == '.') {
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
    /// attribute of its `__init__.py`), not the package itself; `from .pkg
    /// import x` names the module `.pkg`.
    #[test]
    fn python_relative_from_import_names_each_submodule() {
        let src = "import os\nimport a.b.c as x\nfrom . import views, models as m\n\
                   from .. import (up)\nfrom . import *\nfrom .pkg import thing\nfrom mod import y\n";
        assert_eq!(
            modules(src, "python"),
            vec![
                "os", "a.b.c", ".:views", ".:models", "..:up", ".", ".pkg", "mod"
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
        assert!(imports_of("#include <stdio.h>\n", "c").is_empty());
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
        for rel in ["src/lib.rs", ".cabal", "package.json"] {
            assert!(!bears_on_manifests(rel), "{rel}");
        }
    }

    fn dir(name: &str) -> crate::testutil::TempDir {
        crate::testutil::TempDir::new(name)
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
