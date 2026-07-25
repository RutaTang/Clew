//! Symbol outline extraction using tree-sitter tags queries.

use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

// The outline entry is the protocol's wire type: `name`, `kind` ("function",
// "struct", "class", …), `line` (1-based first line), `end_line` (1-based last
// line, for span hashing). Shared so there is no conversion at the wire.
pub use clew_protocol::Symbol;

/// Extract definition symbols from `source`. Returns an empty list when the
/// language has no tags query or parsing fails. Blocking; run off the UI thread.
pub fn extract(source: &str, lang_key: &str) -> Vec<Symbol> {
    let Some((language, tags)) = crate::highlight::tags_for(lang_key) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let Ok(query) = Query::new(&language, tags) else {
        return Vec::new();
    };

    let mut cursor = QueryCursor::new();
    let mut symbols = Vec::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        let mut kind: Option<&str> = None;
        let mut name: Option<&str> = None;
        let mut line = 0usize;
        let mut end_line = 0usize;
        for capture in m.captures {
            let capture_name = query.capture_names()[capture.index as usize];
            if let Some(k) = capture_name.strip_prefix("definition.") {
                kind = Some(k);
                line = capture.node.start_position().row + 1;
                end_line = capture.node.end_position().row + 1;
            } else if capture_name == "name" {
                name = source.get(capture.node.byte_range()).or(name);
            }
        }
        if let (Some(kind), Some(name)) = (kind, name)
            && line > 0
        {
            symbols.push(Symbol {
                name: name.to_string(),
                kind: kind.to_string(),
                line,
                end_line: end_line.max(line),
            });
        }
    }
    symbols.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.name.cmp(&b.name)));
    symbols.dedup_by(|a, b| a.line == b.line && a.name == b.name);
    symbols
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_symbols() {
        let src =
            "pub struct Point { x: f64 }\n\npub fn origin() -> Point {\n    Point { x: 0.0 }\n}\n";
        let symbols = extract(src, "rust");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Point"), "symbols: {symbols:?}");
        assert!(names.contains(&"origin"), "symbols: {symbols:?}");
        let origin = symbols.iter().find(|s| s.name == "origin").unwrap();
        assert_eq!(origin.line, 3);
    }

    #[test]
    fn extracts_dart_symbols() {
        let src = "class Point {\n  final double x;\n  Point(this.x);\n  double get magnitude => x;\n}\n\nPoint origin() => Point(0.0);\n";
        let symbols = extract(src, "dart");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Point"), "symbols: {symbols:?}");
        assert!(names.contains(&"origin"), "symbols: {symbols:?}");
    }

    #[test]
    fn extracts_typescript_source_symbols() {
        let src = "export type Kind = \"a\" | \"b\";\n\
                   export interface Token { kind: Kind }\n\
                   export function tokenize(s: string): Token[] { return []; }\n\
                   const isDigit = (c: string): boolean => c >= \"0\";\n\
                   export class Parser {\n  parse(): number { return 0; }\n}\n";
        let symbols = extract(src, "typescript");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        // The bundled query only found `Token`; the source-oriented one gets all.
        for want in ["Kind", "Token", "tokenize", "isDigit", "Parser", "parse"] {
            assert!(names.contains(&want), "missing {want} in {names:?}");
        }
    }

    #[test]
    fn extracts_typescript_interface_members_and_class_fields() {
        // Interface members (incl. function-typed properties, where libraries put
        // JSDoc) and class fields are surfaced — but an inline object-type
        // property in a parameter annotation is NOT swept in.
        let src = "export interface Chalk {\n\
                   \x20 rgb: (r: number, g: number, b: number) => Chalk;\n\
                   \x20 level: number;\n\
                   \x20 apply(opts: { inline: boolean }): void;\n\
                   }\n\
                   export class Styler {\n  cache = new Map();\n  build() {}\n}\n";
        let symbols = extract(src, "typescript");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        // Interface property members and the class field are present.
        for want in ["rgb", "level", "apply", "cache", "build"] {
            assert!(names.contains(&want), "missing {want} in {names:?}");
        }
        // The inline object-type property `inline` must NOT be captured.
        assert!(
            !names.contains(&"inline"),
            "over-captured inline type prop: {names:?}"
        );
        // Kinds are tagged correctly.
        let kind = |n: &str| {
            symbols
                .iter()
                .find(|s| s.name == n)
                .map(|s| s.kind.as_str())
        };
        assert_eq!(kind("rgb"), Some("property"));
        assert_eq!(kind("level"), Some("property"));
        assert_eq!(kind("cache"), Some("field"));
    }

    #[test]
    fn language_without_tags_query_yields_empty() {
        assert!(extract("{\"a\": 1}", "json").is_empty());
    }
}
