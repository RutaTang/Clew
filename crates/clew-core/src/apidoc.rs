//! The API documentation index for the Docs view.
//!
//! Assembles what clew already extracts — the tree-sitter symbol outline plus
//! per-symbol doc comments — into a browsable, nested API surface for a file:
//! signature, doc, visibility, and members nested under their enclosing type.
//! No build, no language doc tool, no webview — the same six languages clew
//! already parses. The client enriches a selected entry via LSP hover.

use clew_protocol::DocItem;

/// Build the documented API of one file: top-level items, with members nested
/// under their enclosing type/module by source-range containment. Returns an
/// empty list when the language has no outline.
pub fn build_file(source: &str, lang_key: &str) -> Vec<DocItem> {
    let symbols = crate::outline::extract(source, lang_key);
    if symbols.is_empty() {
        return Vec::new();
    }
    let docs = crate::docs::extract_full(source, lang_key, &symbols);
    let lines: Vec<&str> = source.lines().collect();

    // A flat record per symbol, in line order (outline is already sorted).
    struct Raw {
        name: String,
        kind: String,
        line: usize,
        end_line: usize,
        signature: String,
        doc: String,
        public: bool,
    }
    let raws: Vec<Raw> = symbols
        .iter()
        .map(|s| {
            let decl = lines.get(s.line.saturating_sub(1)).copied().unwrap_or("");
            Raw {
                name: s.name.clone(),
                kind: s.kind.clone(),
                line: s.line,
                end_line: s.end_line,
                signature: signature(&lines, s.line),
                doc: docs.get(&s.line).cloned().unwrap_or_default(),
                public: is_public(decl, &s.name, lang_key),
            }
        })
        .collect();

    // Nest by containment: symbol B is a child of the closest earlier symbol A
    // whose range [line, end_line] still encloses B's start line. A stack of
    // open ancestors gives this in one pass.
    let n = raws.len();
    let mut parent: Vec<Option<usize>> = vec![None; n];
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..n {
        while let Some(&top) = stack.last() {
            if raws[top].end_line < raws[i].line {
                stack.pop();
            } else {
                break;
            }
        }
        parent[i] = stack.last().copied();
        stack.push(i);
    }
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots: Vec<usize> = Vec::new();
    for i in 0..n {
        match parent[i] {
            Some(p) => children[p].push(i),
            None => roots.push(i),
        }
    }

    fn build(i: usize, raws: &[Raw], children: &[Vec<usize>]) -> DocItem {
        let r = &raws[i];
        DocItem {
            name: r.name.clone(),
            kind: r.kind.clone(),
            signature: r.signature.clone(),
            doc: r.doc.clone(),
            line: r.line,
            public: r.public,
            children: children[i].iter().map(|&c| build(c, raws, children)).collect(),
        }
    }
    roots.iter().map(|&i| build(i, &raws, &children)).collect()
}

/// The declaration text for the item at `line1` (1-based): join lines from the
/// definition until the body opens (`{`/`;`) or the signature looks complete
/// (balanced parens and not obviously continued), so multi-line signatures are
/// captured but bodies are not.
fn signature(lines: &[&str], line1: usize) -> String {
    let start = line1.saturating_sub(1);
    let mut acc = String::new();
    let mut depth: i32 = 0;
    for l in lines.iter().skip(start).take(8) {
        let cut = l.find(|c| c == '{' || c == ';');
        let seg = match cut {
            Some(i) => &l[..i],
            None => l,
        };
        for c in seg.chars() {
            match c {
                '(' | '[' | '<' => depth += 1,
                ')' | ']' | '>' => depth -= 1,
                _ => {}
            }
        }
        if !acc.is_empty() {
            acc.push(' ');
        }
        acc.push_str(seg.trim());
        if cut.is_some() {
            break;
        }
        let t = seg.trim_end();
        if depth <= 0 && !t.is_empty() && !t.ends_with(',') && !t.ends_with('(') {
            break;
        }
    }
    acc.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether the item is part of the public API, per each language's convention.
/// `decl` is its declaration line, `name` its identifier.
fn is_public(decl: &str, name: &str, lang: &str) -> bool {
    let d = decl.trim_start();
    match lang {
        // Any `pub` (including pub(crate)/pub(super)) counts for the surface.
        "rust" => d.starts_with("pub"),
        // Exported declarations, or class members that aren't explicitly private.
        "typescript" | "tsx" | "javascript" | "jsx" => {
            d.starts_with("export") || !(d.contains("private ") || name.starts_with('#'))
        }
        // Exported = capitalized identifier.
        "go" => name.chars().next().is_some_and(char::is_uppercase),
        // Convention: a leading underscore marks non-public.
        "python" | "dart" => !name.starts_with('_'),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_nests_and_marks_visibility() {
        let src = "\
/// A point.
pub struct Point {
    x: f64,
}

fn helper() {}
";
        let items = build_file(src, "rust");
        // `Point` is public + documented; `helper` is private.
        let point = items.iter().find(|i| i.name == "Point").unwrap();
        assert!(point.public);
        assert!(point.doc.contains("A point"));
        assert!(point.signature.contains("pub struct Point"));
        assert!(items.iter().any(|i| i.name == "helper" && !i.public));
    }

    #[test]
    fn python_methods_nest_under_class() {
        let src = "\
class Greeter:
    def hello(self):
        pass
    def _secret(self):
        pass
";
        let items = build_file(src, "python");
        let cls = items.iter().find(|i| i.name == "Greeter").unwrap();
        assert!(cls.public);
        assert!(cls.children.iter().any(|c| c.name == "hello" && c.public));
        assert!(cls.children.iter().any(|c| c.name == "_secret" && !c.public));
    }

    #[test]
    fn signature_stops_at_body() {
        let lines = vec!["pub fn add(a: i32, b: i32) -> i32 {", "    a + b", "}"];
        assert_eq!(signature(&lines, 1), "pub fn add(a: i32, b: i32) -> i32");
    }
}
