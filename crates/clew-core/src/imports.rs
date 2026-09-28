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
    /// `require()` and dynamic `import()` of a literal), or a Go import path.
    pub module: String,
    /// 1-based line of the statement (for navigation).
    pub line: usize,
    /// Rust `mod x;` — `module` is a submodule name to resolve to a sibling file,
    /// not a scoped path.
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
        Lang::C
        | Lang::Cpp
        | Lang::Java
        | Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
        | Lang::Zig => Vec::new(),
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
        Lang::C
        | Lang::Cpp
        | Lang::Java
        | Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
        | Lang::Zig => None,
    })
}

/// Byte cap for a project metadata file (`go.mod`, `pubspec.yaml`). Both ship
/// with the repository, so their size and type are attacker-chosen and both
/// are read automatically on every project open — a `go.mod -> /dev/zero`
/// would otherwise read until the process died. Real ones are a few KB.
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

fn node_text<'a>(node: Node, src: &'a str) -> &'a str {
    src.get(node.byte_range()).unwrap_or("")
}

fn line_of(node: Node) -> usize {
    node.start_position().row + 1
}

/// Run `query` against `root`, calling `f` once per match with that match's
/// captures resolved to `(capture_name, node)`.
fn for_each_match(root: Node, query: &Query, src: &str, mut f: impl FnMut(&[(&str, Node)])) {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
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

fn capture<'t>(caps: &[(&str, Node<'t>)], name: &str) -> Option<Node<'t>> {
    caps.iter().find(|(n, _)| *n == name).map(|(_, node)| *node)
}

const RUST_QUERY: &str = r#"
(mod_item name: (identifier) @mod_name) @mod_item
(use_declaration argument: (_) @use_arg)
"#;

fn rust_imports(root: Node, src: &str, query: &Query) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, query, src, |caps| {
        if let Some(mi) = capture(caps, "mod_item") {
            // `mod x { ... }` (has a body) is an inline module, not a file edge.
            if mi.child_by_field_name("body").is_some() {
                return;
            }
            if let Some(name) = capture(caps, "mod_name") {
                out.push(RawImport {
                    module: node_text(name, src).to_string(),
                    line: line_of(mi),
                    is_mod_decl: true,
                });
            }
            return;
        }
        if let Some(arg) = capture(caps, "use_arg") {
            let reexport = arg.parent().is_some_and(|decl| is_reexport(decl, src));
            for (module, line) in expand_use_tree(arg, src) {
                out.push(RawImport {
                    module: if reexport {
                        format!("{}{module}", crate::rustscope::REEXPORT)
                    } else {
                        module
                    },
                    line,
                    is_mod_decl: false,
                });
            }
        }
    });
    out
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
fn expand_use_tree(arg: Node, src: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut stack: Vec<(Node, String)> = vec![(arg, String::new())];
    while let Some((node, prefix)) = stack.pop() {
        match node.kind() {
            "scoped_use_list" => {
                let path = node
                    .child_by_field_name("path")
                    .map(|p| path_text(p, src))
                    .unwrap_or_default();
                if let Some(list) = node.child_by_field_name("list") {
                    push_use_list(list, join_path(&prefix, &path), &mut stack);
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
                if !module.is_empty() {
                    let glob = join_path(&module, crate::rustscope::GLOB);
                    out.push((glob, line_of(node)));
                }
            }
            // `use a::{self}`: the list's own prefix.
            "self" if !prefix.is_empty() => out.push((prefix, line_of(node))),
            "line_comment" | "block_comment" => {}
            _ => {
                let module = join_path(&prefix, &path_text(node, src));
                if !module.is_empty() {
                    out.push((module, line_of(node)));
                }
            }
        }
    }
    out
}

/// Queue a `use_list`'s entries so they pop in source order.
fn push_use_list<'t>(list: Node<'t>, prefix: String, stack: &mut Vec<(Node<'t>, String)>) {
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
/// `mod x`; Python's `.:views` becomes `from . import views`. Other
/// languages' specifiers read as written.
pub fn statement_of(raw: &RawImport, lang: Lang) -> String {
    match lang {
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
                            | Lang::Java
                            | Lang::Json
                            | Lang::Bash
                            | Lang::Yaml
                            | Lang::Toml
                            | Lang::Html
                            | Lang::Css
                            | Lang::Zig
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
                let imports = imports_of(&src, "rust");
                assert_eq!(imports.len(), 1);
                assert_eq!(imports[0].module.matches("::").count(), DEPTH);
            })
            .unwrap()
            .join()
            .expect("expanding the use tree must not overflow the stack");
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
        assert!(imports_of("import java.util.List;\n", "java").is_empty());
        assert!(imports_of("x", "klingon").is_empty());
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

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
}
