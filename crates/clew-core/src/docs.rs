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
            doc = clean_doc_text(&doc);
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
/// Clean documentation markup that isn't Markdown so it renders as prose rather
/// than literal noise: dartdoc `{@...}` directives (drop the `{@template}` /
/// `{@endtemplate}` / `{@macro}` markers, keep any text between) and reST
/// cross-reference roles (``:class:`Foo``` → `Foo`, honoring a leading `~`/`.`).
fn clean_doc_text(s: &str) -> String {
    // Pass 1: drop dartdoc `{@ ... }` directive markers.
    let mut a = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find("{@") {
        a.push_str(&rest[..pos]);
        match rest[pos..].find('}') {
            Some(end) => rest = &rest[pos + end + 1..],
            None => {
                rest = &rest[pos + 2..];
            }
        }
    }
    a.push_str(rest);

    // Pass 2: simplify reST roles `:role:`target`` → target.
    let mut out = String::with_capacity(a.len());
    let bytes = a.as_bytes();
    let mut i = 0;
    while i < a.len() {
        if bytes[i] == b':'
            && let Some((shown, consumed)) = rest_role_at(&a[i..])
        {
            out.push_str(&shown);
            i += consumed;
            continue;
        }
        let ch = a[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// If `s` starts with a reST cross-reference role ``:name:`target```, return the
/// simplified target text and the byte length consumed.
fn rest_role_at(s: &str) -> Option<(String, usize)> {
    let after_first = &s[1..]; // past the leading ':'
    // The role name ends at the ':' immediately before the opening backtick, so a
    // domain-qualified role like `py:class` (with an internal colon) is captured.
    let close = after_first.find(":`")?;
    let name = &after_first[..close];
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic() || c == ':') {
        return None;
    }
    let target_part = &after_first[close + 2..]; // past ":`"
    let target_end = target_part.find('`')?;
    let target = &target_part[..target_end];
    let shown = if let Some(t) = target.strip_prefix('~') {
        t.rsplit(['.', ':']).next().unwrap_or(t)
    } else {
        target.strip_prefix('.').unwrap_or(target)
    };
    // leading ':' (1) + name + ":`" (2) + target + closing '`' (1)
    Some((shown.to_string(), 1 + close + 2 + target_end + 1))
}

/// The 0-based index of the first body line of a `def`/`class` whose header
/// begins at `def_idx`. Walks the (possibly multi-line) signature until brackets
/// are balanced and a line ends with the body-opening `:`, then returns the next
/// line — so a wrapped, fully-typed signature doesn't hide its docstring.
fn py_body_start(lines: &[&str], def_idx: usize) -> usize {
    let mut depth: i32 = 0;
    let mut i = def_idx;
    while i < lines.len() {
        for ch in lines[i].chars() {
            match ch {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                _ => {}
            }
        }
        // Ignore a trailing line comment before checking the colon.
        let code = lines[i].split('#').next().unwrap_or(lines[i]).trim_end();
        if depth <= 0 && code.ends_with(':') {
            return i + 1;
        }
        i += 1;
    }
    def_idx + 1
}

fn py_docstring(lines: &[&str], sig_line: usize) -> Option<String> {
    // `sig_line` is the line after the `def` (correct for a single-line header);
    // for a multi-line signature the body — and thus the docstring — is further
    // down, so resolve the real body start from the header line.
    let def_idx = sig_line.saturating_sub(1);
    let mut i = py_body_start(lines, def_idx);
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
    fn cleans_dartdoc_and_rest_directives() {
        // dartdoc: template markers dropped, inner text kept; bare macro dropped.
        assert_eq!(clean_doc_text("{@template x}Hello{@endtemplate}"), "Hello");
        assert_eq!(clean_doc_text("See {@macro foo} here"), "See  here");
        // reST roles simplified to their target, honoring ~ and leading dot.
        assert_eq!(clean_doc_text("A :class:`Request` object"), "A Request object");
        assert_eq!(clean_doc_text("uses :py:class:`Foo`"), "uses Foo");
        assert_eq!(clean_doc_text("call :meth:`~scrapy.Request.replace`"), "call replace");
        // Plain colons (not a role) are untouched.
        assert_eq!(clean_doc_text("note: this is fine"), "note: this is fine");
    }

    #[test]
    fn python_docstring_with_multiline_signature() {
        // A wrapped, fully-typed signature (ubiquitous in modern Python) must not
        // hide the docstring — the requests read-through found these dropped.
        let src = "\
def request(
    method: str,
    url: str,
    **kwargs: Any,
) -> Response:
    \"\"\"Sends an HTTP request.\"\"\"
    return _send(method, url)
";
        let docs = docs_of(src, "python");
        let d = docs.values().next().expect("docstring for the multi-line def");
        assert!(d.contains("Sends an HTTP request"), "{d}");
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
    fn ts_interface_member_jsdoc_is_extracted() {
        // Libraries (e.g. chalk) document their API on interface members; now
        // that members are outline symbols, their JSDoc must be surfaced.
        let src = "export interface Chalk {\n\
                   \x20 /** Sets the foreground to an RGB color. */\n\
                   \x20 rgb: (r: number, g: number, b: number) => Chalk;\n\
                   }\n";
        let docs = docs_of(src, "typescript");
        let all: Vec<&str> = docs.values().map(String::as_str).collect();
        assert!(
            all.iter().any(|d| d.contains("Sets the foreground to an RGB color")),
            "member JSDoc not extracted: {docs:?}"
        );
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
