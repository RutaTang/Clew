//! Per-language doc-comment extraction for the hover "peek".
//!
//! Maps a definition's signature line (1-based) to the documentation the author
//! attached to it, so clew can show a symbol's docs on hover without a language
//! server or an LLM — filling the gap when no LSP is configured. Each language's
//! convention is recognized directly from the text:
//!   Rust    `///` / `//!` line docs above the item
//!   Go      the run of `//` lines immediately above a declaration
//!   JS/TS/Java/C/C++  a `/** … */` (JSDoc / Doxygen) block above
//!   Python  the first string literal inside the body (a docstring)

use std::collections::HashMap;

use crate::outline::Symbol;

/// Longest doc kept, so a runaway block comment can't bloat the tooltip.
const MAX_DOC: usize = 800;

/// Extract `signature_line -> doc_text` for the documented symbols, truncated to
/// `MAX_DOC` for the hover "peek". `symbols` is the already-parsed outline for
/// this file, so no re-parse happens here.
pub fn extract(source: &str, lang_key: &str, symbols: &[Symbol]) -> HashMap<usize, String> {
    extract_with(source, lang_key, symbols, MAX_DOC)
}

/// Like [`extract`] but keeps the full doc text (for the Docs view, which
/// renders it as a page rather than a tooltip).
pub fn extract_full(source: &str, lang_key: &str, symbols: &[Symbol]) -> HashMap<usize, String> {
    extract_with(source, lang_key, symbols, usize::MAX)
}

fn extract_with(
    source: &str,
    lang_key: &str,
    symbols: &[Symbol],
    max: usize,
) -> HashMap<usize, String> {
    let style = DocStyle::for_lang(lang_key);
    if matches!(style, DocStyle::None) || symbols.is_empty() {
        return HashMap::new();
    }
    let lines: Vec<&str> = source.lines().collect();
    let mut out = HashMap::new();
    for s in symbols {
        if out.contains_key(&s.line) {
            continue;
        }
        if let Some(mut doc) = style.doc_for(&lines, s.line) {
            if doc.chars().count() > max {
                doc = doc.chars().take(max).collect::<String>() + "…";
            }
            if !doc.trim().is_empty() {
                out.insert(s.line, doc);
            }
        }
    }
    out
}

enum DocStyle {
    /// Rust: `///` and `//!` line docs above the item.
    RustLike,
    /// Go: the run of `//` lines immediately above the declaration.
    SlashSlash,
    /// JSDoc / Doxygen: a `/** … */` block above.
    Block,
    /// Python: the first string literal inside the body.
    PyDocstring,
    None,
}

impl DocStyle {
    fn for_lang(lang: &str) -> DocStyle {
        match lang {
            // Dart dartdoc uses `///` line comments (same shape as Rust `///`);
            // the `@`-annotation skip in `above_doc` already handles `@override`
            // etc. sitting between the doc and the declaration.
            "rust" | "dart" => DocStyle::RustLike,
            "go" => DocStyle::SlashSlash,
            "javascript" | "typescript" | "tsx" | "java" | "c" | "cpp" => DocStyle::Block,
            "python" => DocStyle::PyDocstring,
            _ => DocStyle::None,
        }
    }

    fn doc_for(&self, lines: &[&str], sig_line: usize) -> Option<String> {
        match self {
            DocStyle::PyDocstring => py_docstring(lines, sig_line),
            DocStyle::None => None,
            _ => self.above_doc(lines, sig_line),
        }
    }

    /// Docs that sit on the lines above a signature (every style but Python).
    fn above_doc(&self, lines: &[&str], sig_line: usize) -> Option<String> {
        if sig_line < 2 {
            return None;
        }
        // 0-based index of the line directly above the signature.
        let mut idx = sig_line as isize - 2;
        // Skip attribute / annotation lines glued to the signature (Rust
        // `#[derive]`, Java `@Override`): the doc sits above them.
        while idx >= 0 {
            let t = lines[idx as usize].trim_start();
            if t.starts_with("#[") || t.starts_with("#!") || t.starts_with('@') {
                idx -= 1;
            } else {
                break;
            }
        }
        if idx < 0 {
            return None;
        }
        let end = idx as usize;
        match self {
            DocStyle::Block => collect_block(lines, end),
            DocStyle::RustLike => collect_line_docs(lines, end, &["///", "//!"]),
            DocStyle::SlashSlash => collect_line_docs(lines, end, &["//"]),
            _ => None,
        }
    }
}

/// Collect a contiguous run of line-comment docs upward from `end`, keeping only
/// lines whose trimmed start matches one of `prefixes`.
fn collect_line_docs(lines: &[&str], end: usize, prefixes: &[&str]) -> Option<String> {
    let mut collected: Vec<String> = Vec::new();
    let mut i = end as isize;
    while i >= 0 {
        let t = lines[i as usize].trim_start();
        let Some(prefix) = prefixes.iter().find(|p| t.starts_with(**p)) else {
            break;
        };
        let body = &t[prefix.len()..];
        let body = body.strip_prefix(' ').unwrap_or(body);
        collected.push(body.trim_end().to_string());
        i -= 1;
    }
    if collected.is_empty() {
        return None;
    }
    collected.reverse();
    let doc = collected.join("\n").trim().to_string();
    (!doc.is_empty()).then_some(doc)
}

/// Collect a `/** … */` block ending on line `end`, stripping the comment
/// markers and leading ` * ` continuations. Returns `None` for a plain `/* */`
/// comment (only `/**` counts as documentation).
fn collect_block(lines: &[&str], end: usize) -> Option<String> {
    if !lines[end].contains("*/") {
        return None;
    }
    let mut start = end as isize;
    while start >= 0 && !lines[start as usize].contains("/*") {
        start -= 1;
    }
    if start < 0 {
        return None;
    }
    let start = start as usize;
    if !lines[start].contains("/**") {
        return None;
    }
    let mut buf: Vec<String> = Vec::new();
    for line in &lines[start..=end] {
        let mut s = line.trim();
        if let Some(p) = s.find("/**") {
            s = s[p + 3..].trim_start();
        } else if let Some(p) = s.find("/*") {
            s = s[p + 2..].trim_start();
        }
        if let Some(p) = s.find("*/") {
            s = s[..p].trim_end();
        }
        let s = s.strip_prefix('*').map(str::trim_start).unwrap_or(s);
        buf.push(s.to_string());
    }
    let doc = buf.join("\n").trim().to_string();
    (!doc.is_empty()).then_some(doc)
}

/// The docstring that opens a Python def/class body: the first string literal
/// after the signature line.
fn py_docstring(lines: &[&str], sig_line: usize) -> Option<String> {
    // Body starts at the 0-based index `sig_line` (the line after the def).
    let mut i = sig_line;
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    let first = lines.get(i)?.trim_start();

    for q in ["\"\"\"", "'''"] {
        if let Some(rest) = first.strip_prefix(q) {
            if let Some(e) = rest.find(q) {
                return non_empty(rest[..e].trim());
            }
            let mut buf = vec![rest.trim_end().to_string()];
            let mut j = i + 1;
            while j < lines.len() {
                if let Some(e) = lines[j].find(q) {
                    buf.push(lines[j][..e].to_string());
                    return non_empty(&dedent(&buf));
                }
                buf.push(lines[j].to_string());
                j += 1;
            }
            return non_empty(&dedent(&buf));
        }
    }
    // A one-line single/double-quoted docstring.
    for q in ["\"", "'"] {
        if let Some(rest) = first.strip_prefix(q)
            && let Some(e) = rest.find(q)
        {
            return non_empty(rest[..e].trim());
        }
    }
    None
}

/// Strip the common leading indentation from a docstring's lines.
fn dedent(lines: &[String]) -> String {
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| if l.len() >= min_indent { &l[min_indent..] } else { l.as_str() })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docs_of(src: &str, lang: &str) -> HashMap<usize, String> {
        let syms = crate::outline::extract(src, lang);
        extract(src, lang, &syms)
    }

    #[test]
    fn rust_line_docs_above_attributes() {
        let src = "/// Adds two numbers.\n/// Returns the sum.\n#[inline]\npub fn add(a: i32, b: i32) -> i32 { a + b }\n";
        let docs = docs_of(src, "rust");
        let d = docs.values().next().expect("a doc");
        assert!(d.contains("Adds two numbers"), "{d}");
        assert!(d.contains("Returns the sum"), "{d}");
    }

    #[test]
    fn rust_plain_comment_is_not_a_doc() {
        let src = "// internal note\npub fn add() {}\n";
        assert!(docs_of(src, "rust").is_empty());
    }

    #[test]
    fn python_docstring_is_extracted() {
        let src = "def greet(name):\n    \"\"\"Say hello to name.\"\"\"\n    return name\n";
        let docs = docs_of(src, "python");
        let d = docs.values().next().expect("a docstring");
        assert!(d.contains("Say hello"), "{d}");
    }

    #[test]
    fn go_leading_slashes_are_docs() {
        let src = "// Add returns the sum.\nfunc Add(a, b int) int {\n\treturn a + b\n}\n";
        let docs = docs_of(src, "go");
        let d = docs.values().next().expect("a doc");
        assert!(d.contains("Add returns the sum"), "{d}");
    }

    #[test]
    fn js_block_doc_is_extracted() {
        let src = "/**\n * Adds two numbers.\n */\nfunction add(a, b) { return a + b }\n";
        let docs = docs_of(src, "javascript");
        let d = docs.values().next().expect("a doc");
        assert!(d.contains("Adds two numbers"), "{d}");
    }

    #[test]
    fn dart_dartdoc_is_extracted() {
        // Dart uses `///` line docs; the `@`-annotation skip lets the doc sit
        // above a `@Deprecated(...)` line and still be found.
        let src = "\
/// A parser for command-line arguments.\n\
class ArgParser {\n\
  /// Adds a boolean flag.\n\
  @Deprecated('use addOption')\n\
  void addFlag(String name) {}\n\
}\n";
        let docs = docs_of(src, "dart");
        let all: Vec<&str> = docs.values().map(String::as_str).collect();
        assert!(all.iter().any(|d| d.contains("A parser for command-line arguments")), "class doc: {docs:?}");
        assert!(all.iter().any(|d| d.contains("Adds a boolean flag")), "method doc above @annotation: {docs:?}");
    }
}
