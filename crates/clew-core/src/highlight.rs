//! Language registry and tree-sitter based syntax highlighting.
//!
//! Highlighting output is a list of lines, each line a list of
//! `(text, style index)` spans. The style index points into
//! [`HIGHLIGHT_NAMES`]; mapping an index to a color is the client's job (the
//! theme lives on the GUI side), so this crate stays GUI-free and the same
//! tokenizer runs in the client and in the headless server.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use tree_sitter::Language;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// Capture names recognized by the highlighter. Index = style id.
pub const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "embedded",
    "escape",
    "function",
    "function.builtin",
    "function.method",
    "keyword",
    "label",
    "module",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "string",
    "string.escape",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

/// One source line as a list of `(text, style index)` spans.
///
/// This is the protocol's wire type for a highlighted line — the tokenizer
/// produces exactly what the client renders and the server transmits, with no
/// conversion in between.
pub use clew_protocol::HlLine;

struct LangDef {
    name: &'static str,
    language: fn() -> Language,
    highlights: &'static str,
    tags: Option<&'static str>,
}

/// Source-oriented tags query for TypeScript/TSX — captures the symbols in real
/// `.ts` source (classes, functions, methods, type aliases, enums, interfaces,
/// arrow-function consts), which the crate's declaration-focused query misses.
const TS_TAGS: &str = r#"
(class_declaration name: (type_identifier) @name) @definition.class
(abstract_class_declaration name: (type_identifier) @name) @definition.class
(function_declaration name: (identifier) @name) @definition.function
(generator_function_declaration name: (identifier) @name) @definition.function
(function_signature name: (identifier) @name) @definition.function
(method_definition name: (property_identifier) @name) @definition.method
(method_signature name: (property_identifier) @name) @definition.method
(abstract_method_signature name: (property_identifier) @name) @definition.method
(interface_declaration name: (type_identifier) @name) @definition.interface
(type_alias_declaration name: (type_identifier) @name) @definition.type
(enum_declaration name: (identifier) @name) @definition.enum
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @definition.function
;; Interface members and class fields, scoped to the interface/class body so
;; inline object-type properties (e.g. a `{a: number}` parameter annotation)
;; are not swept in. This matches the VS Code / Zed outline, and surfaces the
;; JSDoc that libraries put on function-typed members (chalk's `rgb`, `hex`…).
(interface_body
  (property_signature name: (property_identifier) @name) @definition.property)
(class_body
  (public_field_definition name: (property_identifier) @name) @definition.field)
"#;

/// Source-oriented tags query for Rust. The crate's bundled query lumps every
/// ADT — struct, enum, union, and type alias — under `@definition.class`, so the
/// outline mislabels a Rust `enum` as "class". This mirrors the bundled query
/// (same patterns / order, so method-vs-function dedup is unchanged) but tags
/// each ADT and traits with their real kind.
const RUST_TAGS: &str = r#"
(struct_item name: (type_identifier) @name) @definition.struct
(enum_item name: (type_identifier) @name) @definition.enum
(union_item name: (type_identifier) @name) @definition.union
(type_item name: (type_identifier) @name) @definition.type
(declaration_list
    (function_item name: (identifier) @name) @definition.method)
(function_item name: (identifier) @name) @definition.function
(trait_item name: (type_identifier) @name) @definition.trait
(mod_item name: (identifier) @name) @definition.module
(macro_definition name: (identifier) @name) @definition.macro
"#;

fn lang_def(key: &str) -> Option<LangDef> {
    use tree_sitter_typescript as ts;
    Some(match key {
        "rust" => LangDef {
            name: "Rust",
            language: || tree_sitter_rust::LANGUAGE.into(),
            highlights: tree_sitter_rust::HIGHLIGHTS_QUERY,
            tags: Some(RUST_TAGS),
        },
        "python" => LangDef {
            name: "Python",
            language: || tree_sitter_python::LANGUAGE.into(),
            highlights: tree_sitter_python::HIGHLIGHTS_QUERY,
            tags: Some(tree_sitter_python::TAGS_QUERY),
        },
        "javascript" => LangDef {
            name: "JavaScript",
            language: || tree_sitter_javascript::LANGUAGE.into(),
            highlights: tree_sitter_javascript::HIGHLIGHT_QUERY,
            tags: Some(tree_sitter_javascript::TAGS_QUERY),
        },
        "typescript" => LangDef {
            name: "TypeScript",
            language: || ts::LANGUAGE_TYPESCRIPT.into(),
            highlights: ts::HIGHLIGHTS_QUERY,
            // The crate's bundled TAGS_QUERY only captures declaration-file
            // constructs (function_signature, method_signature, interface, …),
            // so real .ts source (class/function/method/type) yields almost no
            // symbols. Use a source-oriented tags query instead.
            tags: Some(TS_TAGS),
        },
        "tsx" => LangDef {
            name: "TSX",
            language: || ts::LANGUAGE_TSX.into(),
            highlights: ts::HIGHLIGHTS_QUERY,
            tags: Some(TS_TAGS),
        },
        "go" => LangDef {
            name: "Go",
            language: || tree_sitter_go::LANGUAGE.into(),
            highlights: tree_sitter_go::HIGHLIGHTS_QUERY,
            tags: Some(tree_sitter_go::TAGS_QUERY),
        },
        "dart" => LangDef {
            name: "Dart",
            language: || tree_sitter_dart_orchard::LANGUAGE.into(),
            highlights: tree_sitter_dart_orchard::HIGHLIGHTS_QUERY,
            tags: Some(tree_sitter_dart_orchard::TAGS_QUERY),
        },
        "c" => LangDef {
            name: "C",
            language: || tree_sitter_c::LANGUAGE.into(),
            highlights: tree_sitter_c::HIGHLIGHT_QUERY,
            tags: Some(tree_sitter_c::TAGS_QUERY),
        },
        "cpp" => LangDef {
            name: "C++",
            language: || tree_sitter_cpp::LANGUAGE.into(),
            highlights: tree_sitter_cpp::HIGHLIGHT_QUERY,
            tags: Some(tree_sitter_cpp::TAGS_QUERY),
        },
        "java" => LangDef {
            name: "Java",
            language: || tree_sitter_java::LANGUAGE.into(),
            highlights: tree_sitter_java::HIGHLIGHTS_QUERY,
            tags: Some(tree_sitter_java::TAGS_QUERY),
        },
        "json" => LangDef {
            name: "JSON",
            language: || tree_sitter_json::LANGUAGE.into(),
            highlights: tree_sitter_json::HIGHLIGHTS_QUERY,
            tags: None,
        },
        "bash" => LangDef {
            name: "Shell",
            language: || tree_sitter_bash::LANGUAGE.into(),
            highlights: tree_sitter_bash::HIGHLIGHT_QUERY,
            tags: None,
        },
        "yaml" => LangDef {
            name: "YAML",
            language: || tree_sitter_yaml::LANGUAGE.into(),
            highlights: tree_sitter_yaml::HIGHLIGHTS_QUERY,
            tags: None,
        },
        "toml" => LangDef {
            name: "TOML",
            language: || tree_sitter_toml_ng::LANGUAGE.into(),
            highlights: tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            tags: None,
        },
        "html" => LangDef {
            name: "HTML",
            language: || tree_sitter_html::LANGUAGE.into(),
            highlights: tree_sitter_html::HIGHLIGHTS_QUERY,
            tags: None,
        },
        "css" => LangDef {
            name: "CSS",
            language: || tree_sitter_css::LANGUAGE.into(),
            highlights: tree_sitter_css::HIGHLIGHTS_QUERY,
            tags: None,
        },
        "zig" => LangDef {
            name: "Zig",
            language: || tree_sitter_zig::LANGUAGE.into(),
            highlights: tree_sitter_zig::HIGHLIGHTS_QUERY,
            tags: None,
        },
        _ => return None,
    })
}

/// Detect the language key from a file path (by extension).
pub fn detect(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "go" => "go",
        "dart" => "dart",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "ipp" => "cpp",
        "java" => "java",
        "json" | "jsonc" => "json",
        "sh" | "bash" | "zsh" => "bash",
        "yml" | "yaml" => "yaml",
        "toml" => "toml",
        "html" | "htm" => "html",
        "css" => "css",
        "zig" => "zig",
        _ => return None,
    })
}

/// Language key for a markdown fence tag (```rust, ```py, …), accepting the
/// common aliases LLMs emit. `None` for unknown/absent tags → plain rendering.
pub fn lang_for_fence(tag: &str) -> Option<&'static str> {
    Some(match tag.trim().to_lowercase().as_str() {
        "rust" | "rs" => "rust",
        "python" | "py" => "python",
        "javascript" | "js" | "jsx" => "javascript",
        "typescript" | "ts" => "typescript",
        "tsx" => "tsx",
        "go" | "golang" => "go",
        "dart" => "dart",
        "c" | "h" => "c",
        "cpp" | "c++" | "cxx" => "cpp",
        "java" => "java",
        "json" | "jsonc" => "json",
        "sh" | "bash" | "shell" | "zsh" | "console" => "bash",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "html" => "html",
        "css" => "css",
        "zig" => "zig",
        _ => return None,
    })
}

/// Human-readable language name for a key.
pub fn lang_name(key: &str) -> Option<&'static str> {
    lang_def(key).map(|d| d.name)
}

/// Language + tags query used by the outline extractor.
pub fn tags_for(key: &str) -> Option<(Language, &'static str)> {
    let def = lang_def(key)?;
    Some(((def.language)(), def.tags?))
}

/// Bare tree-sitter grammar for a language key, for callers that run their own
/// queries (e.g. the import extractor).
pub fn language_for(key: &str) -> Option<Language> {
    lang_def(key).map(|d| (d.language)())
}

static CONFIGS: LazyLock<Mutex<HashMap<&'static str, Arc<HighlightConfiguration>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn config_for(key: &'static str) -> Option<Arc<HighlightConfiguration>> {
    let mut map = CONFIGS.lock().unwrap();
    if let Some(config) = map.get(key) {
        return Some(config.clone());
    }
    let def = lang_def(key)?;
    let mut config =
        HighlightConfiguration::new((def.language)(), def.name, def.highlights, "", "").ok()?;
    config.configure(HIGHLIGHT_NAMES);
    let config = Arc::new(config);
    map.insert(key, config.clone());
    Some(config)
}

/// Normalize a text fragment for display: strip CR, expand tabs.
fn clean(s: &str) -> String {
    s.replace('\r', "").replace('\t', "    ")
}

/// Split a source file into unstyled lines.
pub fn plain_lines(source: &str) -> Vec<HlLine> {
    source
        .lines()
        .map(|l| HlLine {
            spans: vec![(clean(l), None)],
        })
        .collect()
}

/// Highlight a source file into per-line styled spans.
/// Falls back to plain lines when the language is unknown or parsing fails.
pub fn highlight_lines(source: &str, lang_key: Option<&'static str>) -> Vec<HlLine> {
    lang_key
        .and_then(config_for)
        .and_then(|config| highlight_with(source, &config))
        .unwrap_or_else(|| plain_lines(source))
}

fn highlight_with(source: &str, config: &HighlightConfiguration) -> Option<Vec<HlLine>> {
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(config, source.as_bytes(), None, |_| None)
        .ok()?;

    let mut lines: Vec<HlLine> = Vec::new();
    let mut current = HlLine::default();
    let mut stack: Vec<u8> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(h) => stack.push(h.0 as u8),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let style = stack.last().copied();
                let mut rest = source.get(start..end)?;
                while let Some(pos) = rest.find('\n') {
                    let (head, tail) = rest.split_at(pos);
                    if !head.is_empty() {
                        current.spans.push((clean(head), style));
                    }
                    lines.push(std::mem::take(&mut current));
                    rest = &tail[1..];
                }
                if !rest.is_empty() {
                    current.spans.push((clean(rest), style));
                }
            }
        }
    }
    if !current.spans.is_empty() {
        lines.push(current);
    }
    Some(lines)
}

/// Whether a style index denotes a string, comment or escape scope — i.e. text
/// where brackets and identifiers should not be treated as code. Used by the
/// bracket matcher and occurrence highlighter to ignore literal text.
pub fn style_is_literal(idx: u8) -> bool {
    HIGHLIGHT_NAMES.get(idx as usize).is_some_and(|name| {
        name.starts_with("comment") || name.starts_with("string") || *name == "escape"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_SRC: &str = "/// Doc comment.\npub fn add(a: i32, b: i32) -> i32 {\n\tlet s = \"x\\n\";\n    a + b\n}\n\nstruct Point {\n    x: f64,\n}\n";

    #[test]
    fn highlighted_line_count_matches_plain() {
        let plain = plain_lines(RUST_SRC);
        let highlighted = highlight_lines(RUST_SRC, Some("rust"));
        assert_eq!(plain.len(), highlighted.len());
        assert_eq!(plain.len(), RUST_SRC.lines().count());
    }

    #[test]
    fn highlighting_produces_styled_spans() {
        let lines = highlight_lines(RUST_SRC, Some("rust"));
        let styled = lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|(_, style)| style.is_some())
            .count();
        assert!(styled > 3, "expected styled spans, got {styled}");
    }

    #[test]
    fn line_text_is_preserved() {
        let lines = highlight_lines(RUST_SRC, Some("rust"));
        let reassembled: String = lines[3].spans.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(reassembled, "    a + b");
    }

    #[test]
    fn tabs_and_crlf_are_cleaned() {
        let lines = plain_lines("a\tb\r\nc\r\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].0, "a    b");
        assert_eq!(lines[1].spans[0].0, "c");
    }

    #[test]
    fn unknown_language_falls_back_to_plain() {
        let lines = highlight_lines("hello\nworld", None);
        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .all(|l| l.spans.iter().all(|(_, s)| s.is_none()))
        );
    }

    #[test]
    fn detect_by_extension() {
        assert_eq!(detect(Path::new("a/b.rs")), Some("rust"));
        assert_eq!(detect(Path::new("x.tsx")), Some("tsx"));
        assert_eq!(detect(Path::new("x.zig")), Some("zig"));
        assert_eq!(detect(Path::new("x.unknown")), None);
    }

    #[test]
    fn zig_highlights() {
        let src = "const std = @import(\"std\");\npub fn main() void {}\n";
        let lines = highlight_lines(src, Some("zig"));
        assert_eq!(lines.len(), 2);
        let styled = lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|(_, s)| s.is_some())
            .count();
        assert!(styled > 2, "expected Zig highlighting, got {styled}");
        assert_eq!(lang_name("zig"), Some("Zig"));
    }
}
