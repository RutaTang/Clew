//! Symbol outline extraction using tree-sitter tags queries.

use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: String, // "function", "struct", "class", ...
    pub line: usize,  // 1-based
}

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
        for capture in m.captures {
            let capture_name = query.capture_names()[capture.index as usize];
            if let Some(k) = capture_name.strip_prefix("definition.") {
                kind = Some(k);
                line = capture.node.start_position().row + 1;
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
        let src = "pub struct Point { x: f64 }\n\npub fn origin() -> Point {\n    Point { x: 0.0 }\n}\n";
        let symbols = extract(src, "rust");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Point"), "symbols: {symbols:?}");
        assert!(names.contains(&"origin"), "symbols: {symbols:?}");
        let origin = symbols.iter().find(|s| s.name == "origin").unwrap();
        assert_eq!(origin.line, 3);
    }

    #[test]
    fn language_without_tags_query_yields_empty() {
        assert!(extract("{\"a\": 1}", "json").is_empty());
    }
}
