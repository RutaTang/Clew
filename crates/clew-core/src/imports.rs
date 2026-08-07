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
use tree_sitter::{Node, Parser, Query, QueryCursor};

/// One import as written in a file, before resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawImport {
    /// The specifier text, normalized per language: a Rust `use` path
    /// (`crate::lsp::client`), a Python dotted/relative module (`os.path`,
    /// `.sibling`), a JS/TS specifier (`./util`, `react`), or a Go import path.
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
    let Some(language) = crate::highlight::language_for(lang) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let root = tree.root_node();
    match lang {
        "rust" => rust_imports(root, source, &language),
        "python" => python_imports(root, source, &language),
        "javascript" | "typescript" | "tsx" => js_imports(root, source, &language),
        "go" => go_imports(root, source, &language),
        "dart" => dart_imports(root, source, &language),
        _ => Vec::new(),
    }
}

/// The `module` line from a project root's `go.mod`, if any. Shared so the
/// server can ship it to a remote client (whose resolver must not read the
/// remote-pathed file off the local disk).
pub fn read_go_module(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("go.mod")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("module ") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// The package `name:` from `pubspec.yaml` — a top-level (unindented) key, so a
/// nested `name:` under `dependencies:` isn't mistaken for it.
pub fn read_dart_package(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("pubspec.yaml")).ok()?;
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

/// Run `query_src` against `root`, calling `f` once per match with that match's
/// captures resolved to `(capture_name, node)`.
fn for_each_match(
    root: Node,
    language: &tree_sitter::Language,
    query_src: &str,
    src: &str,
    mut f: impl FnMut(&[(&str, Node)]),
) {
    let Ok(query) = Query::new(language, query_src) else {
        return;
    };
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, src.as_bytes());
    while let Some(m) = matches.next() {
        let caps: Vec<(&str, Node)> = m
            .captures
            .iter()
            .map(|c| (names[c.index as usize], c.node))
            .collect();
        f(&caps);
    }
}

const RUST_QUERY: &str = r#"
(mod_item name: (identifier) @mod_name) @mod_item
(use_declaration argument: (_) @use_arg)
"#;

fn rust_imports(root: Node, src: &str, language: &tree_sitter::Language) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, language, RUST_QUERY, src, |caps| {
        let mod_item = caps.iter().find(|(n, _)| *n == "mod_item").map(|(_, n)| *n);
        if let Some(mi) = mod_item {
            // `mod x { ... }` (has a body) is an inline module, not a file edge.
            if mi.child_by_field_name("body").is_some() {
                return;
            }
            if let Some((_, name)) = caps.iter().find(|(n, _)| *n == "mod_name") {
                out.push(RawImport {
                    module: node_text(*name, src).to_string(),
                    line: mi.start_position().row + 1,
                    is_mod_decl: true,
                });
            }
            return;
        }
        if let Some((_, arg)) = caps.iter().find(|(n, _)| *n == "use_arg") {
            let path = rust_use_path(node_text(*arg, src));
            if !path.is_empty() {
                out.push(RawImport {
                    module: path,
                    line: arg.start_position().row + 1,
                    is_mod_decl: false,
                });
            }
        }
    });
    out
}

/// Normalize a `use` tree's text to its leading module path: cut a grouped
/// `{...}`, an `as` alias or a `*` glob, and trim a trailing `::`. Grouped
/// imports collapse to their common module prefix (a safe over-approximation;
/// LSP refinement can split them later).
fn rust_use_path(text: &str) -> String {
    let mut s = text.trim();
    if let Some(i) = s.find('{') {
        s = s[..i].trim_end();
    }
    if let Some(i) = s.find(" as ") {
        s = s[..i].trim_end();
    }
    s = s.trim_end_matches('*').trim_end();
    s.trim_end_matches("::").trim().to_string()
}

const PYTHON_QUERY: &str = r#"
(import_statement name: (dotted_name) @plain)
(import_statement name: (aliased_import name: (dotted_name) @plain))
(import_from_statement module_name: (_) @from)
"#;

fn python_imports(root: Node, src: &str, language: &tree_sitter::Language) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, language, PYTHON_QUERY, src, |caps| {
        for (name, node) in caps {
            let text = node_text(*node, src).trim().to_string();
            if text.is_empty() {
                continue;
            }
            // `plain` is always absolute; `from` may carry leading dots (relative).
            let _ = name;
            out.push(RawImport {
                module: text,
                line: node.start_position().row + 1,
                is_mod_decl: false,
            });
        }
    });
    out
}

const JS_QUERY: &str = r#"
(import_statement source: (string) @src)
(export_statement source: (string) @src)
"#;

fn js_imports(root: Node, src: &str, language: &tree_sitter::Language) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, language, JS_QUERY, src, |caps| {
        if let Some((_, node)) = caps.iter().find(|(n, _)| *n == "src") {
            let spec = strip_quotes(node_text(*node, src));
            if !spec.is_empty() {
                out.push(RawImport {
                    module: spec.to_string(),
                    line: node.start_position().row + 1,
                    is_mod_decl: false,
                });
            }
        }
    });
    out
}

const GO_QUERY: &str = r#"
(import_spec path: (interpreted_string_literal) @path)
(import_spec path: (raw_string_literal) @path)
"#;

fn go_imports(root: Node, src: &str, language: &tree_sitter::Language) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, language, GO_QUERY, src, |caps| {
        if let Some((_, node)) = caps.iter().find(|(n, _)| *n == "path") {
            let spec = strip_quotes(node_text(*node, src));
            if !spec.is_empty() {
                out.push(RawImport {
                    module: spec.to_string(),
                    line: node.start_position().row + 1,
                    is_mod_decl: false,
                });
            }
        }
    });
    out
}

// Dart `import`/`export` both carry the target as a `uri (string_literal)`,
// either directly or wrapped in a `configurable_uri` (for `if (dart.library.io)`
// conditional imports).
const DART_QUERY: &str = r#"
(library_import (import_specification (uri (string_literal) @path)))
(library_import (import_specification (configurable_uri (uri (string_literal) @path))))
(library_export (configurable_uri (uri (string_literal) @path)))
"#;

fn dart_imports(root: Node, src: &str, language: &tree_sitter::Language) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, language, DART_QUERY, src, |caps| {
        if let Some((_, node)) = caps.iter().find(|(n, _)| *n == "path") {
            let spec = strip_quotes(node_text(*node, src));
            if !spec.is_empty() {
                out.push(RawImport {
                    module: spec.to_string(),
                    line: node.start_position().row + 1,
                    is_mod_decl: false,
                });
            }
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
