//! Per-language doc-comment extraction for the hover "peek".
//!
//! Maps a definition's signature line (1-based) to the documentation the author
//! attached to it, so clew can show a symbol's docs on hover without a language
//! server or an LLM — filling the gap when no LSP is configured. Each language's
//! convention is recognized directly from the text:
//!   Rust    `///` / `//!` line docs above the item
//!   Go      the run of `//` lines immediately above a declaration
//!   JS/TS/Java/C/C++  a `/** … */` (JSDoc / Doxygen) block above
//!   Python  the first string literal inside a def/class body (a docstring)

use std::collections::HashMap;

use crate::highlight::Lang;
use crate::outline::{Located, Owner, Symbol, work};

/// Longest doc kept, so a runaway block comment can't bloat the tooltip.
const MAX_DOC: usize = 800;

/// One symbol to document: the line its doc is reported under (`key`), the
/// line the declaration starts on (`anchor` — the doc sits directly above
/// it), what it is, and — when known — who it belongs to.
struct Target<'a> {
    key: usize,
    anchor: usize,
    kind: &'a str,
    name: &'a str,
    /// `None` until an overload group needs it (see [`share_overload_docs`]);
    /// a located outline knows it up front.
    owner: Option<Owner>,
}

fn targets_of(symbols: &[Symbol]) -> Vec<Target<'_>> {
    symbols
        .iter()
        .map(|s| Target {
            key: s.line,
            anchor: s.line,
            kind: &s.kind,
            name: &s.name,
            owner: None,
        })
        .collect()
}

/// Extract `signature_line -> doc_text` for the documented symbols, truncated to
/// `MAX_DOC` for the hover "peek". `symbols` is the already-parsed outline for
/// this file, so no re-parse happens here.
pub fn extract(source: &str, lang_key: &str, symbols: &[Symbol]) -> HashMap<usize, String> {
    extract_with(source, lang_key, targets_of(symbols), MAX_DOC)
}

/// Like [`extract`] but keeps the full doc text (for the Docs view, which
/// renders it as a page rather than a tooltip).
pub fn extract_full(source: &str, lang_key: &str, symbols: &[Symbol]) -> HashMap<usize, String> {
    extract_with(source, lang_key, targets_of(symbols), usize::MAX)
}

/// [`extract_full`] over a located outline: each doc is looked for above the
/// line the DECLARATION starts on, which is not always the name's line — a
/// GNU-style C definition puts `static int` on the line above `foo(void)`, and
/// its doc comment above that. Results are still keyed by `symbol.line`.
pub fn extract_located(source: &str, lang_key: &str, items: &[Located]) -> HashMap<usize, String> {
    let targets = items
        .iter()
        .map(|l| Target {
            key: l.symbol.line,
            anchor: l.decl_line,
            kind: &l.symbol.kind,
            name: &l.symbol.name,
            owner: Some(l.owner.clone()),
        })
        .collect();
    extract_with(source, lang_key, targets, usize::MAX)
}

fn extract_with(
    source: &str,
    lang_key: &str,
    mut targets: Vec<Target>,
    max: usize,
) -> HashMap<usize, String> {
    let Some(lang) = Lang::for_source(lang_key, source) else {
        return HashMap::new();
    };
    let style = DocStyle::for_lang(lang);
    if matches!(style, DocStyle::None) || targets.is_empty() {
        return HashMap::new();
    }
    let lines: Vec<&str> = source.lines().collect();
    // Only the block styles consult it, and building it costs a scan of every
    // line, so the line-comment and docstring languages do not pay for it.
    let openers = match style {
        DocStyle::Block => block_openers(&lines),
        _ => Vec::new(),
    };
    let mut out = HashMap::new();
    for t in &targets {
        if out.contains_key(&t.key) {
            continue;
        }
        if let Some(mut doc) = style.doc_for(&lines, &openers, t) {
            doc = clean_doc_text(&doc, lang);
            if doc.chars().count() > max {
                doc = doc.chars().take(max).collect::<String>() + "…";
            }
            if !doc.trim().is_empty() {
                out.insert(t.key, doc);
            }
        }
    }
    if has_overloading(lang) {
        share_overload_docs(source, lang, &mut targets, &mut out);
    }
    out
}

/// Overloads: the doc sits above the first overload signature, so only that
/// symbol picks it up — the implementation below shares the name but has no
/// doc of its own and would render "No documentation". So a group of
/// overloads shares its doc with its members.
///
/// A group is a run of same-name symbols, adjacent in line order, with one
/// [`Owner`] ([`Owner::same_as`]): one class, namespace or body — so a C++
/// member declared in its class shares with its definition after the class.
/// The name and adjacency alone let a doc leak between unrelated neighbours —
/// C++ `A::draw` right after `B::draw`, or the `run` entries of two object
/// literals — and adjacency alone keeps two classes' `get`s apart only
/// because the second class usually sits between them.
///
/// Only in languages that HAVE overloading. In Rust and Go two adjacent
/// same-name functions are methods of two different types (`impl A { fn new }`
/// right after `impl B { fn new }`), and sharing handed B's constructor A's
/// documentation.
fn share_overload_docs(
    source: &str,
    lang: Lang,
    targets: &mut [Target],
    out: &mut HashMap<usize, String>,
) {
    targets.sort_by_key(|t| t.key);
    let candidate = targets.windows(2).any(|w| w[0].name == w[1].name);
    if !candidate {
        return;
    }
    // A plain outline carries no owners; read them off the tree once, only
    // for a file that has a candidate group at all.
    if targets.iter().any(|t| t.owner.is_none()) {
        let mut owners: HashMap<(usize, &str), Owner> = HashMap::new();
        let located = crate::outline::extract_located(source, lang.key());
        for l in &located {
            owners.insert((l.symbol.line, l.symbol.name.as_str()), l.owner.clone());
        }
        for t in targets.iter_mut().filter(|t| t.owner.is_none()) {
            t.owner = owners.get(&(t.key, t.name)).cloned();
        }
    }
    // An unknown owner matches nothing: not sharing costs a missing doc,
    // sharing wrongly shows another symbol's.
    let same_group = |a: &Target, b: &Target| {
        a.name == b.name
            && match (&a.owner, &b.owner) {
                (Some(x), Some(y)) => x.same_as(y),
                _ => false,
            }
    };
    let mut i = 0;
    while i < targets.len() {
        let mut j = i;
        while j + 1 < targets.len() && same_group(&targets[j], &targets[j + 1]) {
            j += 1;
        }
        if j > i
            && let Some(doc) = (i..=j).find_map(|k| out.get(&targets[k].key).cloned())
        {
            for t in &targets[i..=j] {
                out.entry(t.key).or_insert_with(|| doc.clone());
            }
        }
        i = j + 1;
    }
}

/// Whether one name can declare several functions side by side that are the
/// SAME API: TypeScript overload signatures, Java and C++ overloads, Python
/// `@overload` stubs and property getter/setter pairs.
fn has_overloading(lang: Lang) -> bool {
    match lang {
        Lang::TypeScript | Lang::Tsx | Lang::Java | Lang::Cpp | Lang::Python => true,
        Lang::Rust
        | Lang::JavaScript
        | Lang::Go
        | Lang::Dart
        | Lang::C
        | Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
        | Lang::Zig => false,
    }
}

enum DocStyle {
    /// Rust: `///` and `//!` line docs above the item.
    RustLike,
    /// Go: the run of `//` lines immediately above the declaration.
    SlashSlash,
    /// JSDoc / Doxygen: a `/** … */` block above.
    Block,
    /// Python: the first string literal inside a def/class body.
    PyDocstring,
    None,
}

impl DocStyle {
    fn for_lang(lang: Lang) -> DocStyle {
        match lang {
            // Dart dartdoc uses `///` line comments (same shape as Rust `///`);
            // the `@`-annotation skip in `above_doc` already handles `@override`
            // etc. sitting between the doc and the declaration.
            Lang::Rust | Lang::Dart => DocStyle::RustLike,
            Lang::Go => DocStyle::SlashSlash,
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx | Lang::Java | Lang::C | Lang::Cpp => {
                DocStyle::Block
            }
            Lang::Python => DocStyle::PyDocstring,
            Lang::Json
            | Lang::Bash
            | Lang::Yaml
            | Lang::Toml
            | Lang::Html
            | Lang::Css
            | Lang::Zig => DocStyle::None,
        }
    }

    fn doc_for(
        &self,
        lines: &[&str],
        openers: &[Option<usize>],
        target: &Target,
    ) -> Option<String> {
        match self {
            // Only a def or class HAS a docstring. A module constant is not
            // followed by one, and looking for it walked forward to the next
            // `def` — giving `VERSION = "1.0"` that function's docstring, and
            // costing a scan of the rest of the file per constant.
            DocStyle::PyDocstring => matches!(target.kind, "function" | "method" | "class")
                .then(|| py_docstring(lines, target.anchor))
                .flatten(),
            DocStyle::None => None,
            _ => self.above_doc(lines, openers, target.anchor),
        }
    }

    /// Docs that sit on the lines above a signature (every style but Python).
    /// `openers` is the block-comment index, empty for the styles that never
    /// look at it.
    fn above_doc(
        &self,
        lines: &[&str],
        openers: &[Option<usize>],
        sig_line: usize,
    ) -> Option<String> {
        if sig_line < 2 || sig_line > lines.len() + 1 {
            return None;
        }
        // 0-based index of the line directly above the signature.
        let mut idx = sig_line as isize - 2;
        // Skip lines glued between the doc and the signature: attribute /
        // annotation lines (Rust `#[derive]`, Java `@Override`) and single-line
        // non-doc block-comment pragmas (Rollup's `/*@__NO_SIDE_EFFECTS__*/`,
        // `/*#__PURE__*/` — ubiquitous above exported functions in JS/TS libs).
        // Without the pragma skip, that `/* … */` masks the real JSDoc above it.
        while idx >= 0 {
            let t = lines[idx as usize].trim();
            let is_annotation = t.starts_with("#[") || t.starts_with("#!") || t.starts_with('@');
            let is_pragma_comment =
                t.starts_with("/*") && !t.starts_with("/**") && t.ends_with("*/");
            if is_annotation || is_pragma_comment {
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
            DocStyle::Block => collect_block(lines, openers, end),
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
        work::add(1);
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

/// For every line, the nearest line at or above it that contains a
/// block-comment opener `/*`, or `None` when there is none.
///
/// This is precisely the line `collect_block` used to find by walking UPWARD
/// from each symbol, one line at a time, stopping only at `/*` or at line 0.
/// That scan does not stop at the previous symbol and nothing memoized it, so a
/// file where every symbol is preceded by a line that merely CONTAINS `*/` and
/// holds no `/*` anywhere paid a full-file scan per symbol — O(symbols × lines).
/// The trigger needs no invalid syntax: `const s = "*/";` is legal JavaScript,
/// and 4 MiB of that shape (the ReadFile cap) took over five minutes inside the
/// blocking task, so the file's pane stayed empty with no error for that long.
/// One forward pass answers the same question in O(1) per symbol, and returns
/// the identical line for every input.
fn block_openers(lines: &[&str]) -> Vec<Option<usize>> {
    work::add(lines.len());
    let mut out = Vec::with_capacity(lines.len());
    let mut last: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if l.contains("/*") {
            last = Some(i);
        }
        out.push(last);
    }
    out
}

/// How far above a symbol its documentation may open. A doc comment is a local
/// construct, so when the nearest `/*` is farther than this the `*/` that led
/// here is far likelier a string literal than the close of this symbol's docs.
///
/// This is a real limit, not just a guard: a genuine `/** … */` spanning more
/// than this many lines stops being reported as documentation. It also bounds
/// the WORK, which the index alone does not — the block is joined line by line
/// into an owned string per symbol, so a single unterminated `/**` at the top
/// of a file with thousands of `*/` lines below it would otherwise build a
/// near-file-sized doc string for every one of them, quadratic in both time and
/// memory.
const MAX_BLOCK_LINES: usize = 512;

/// Collect a `/** … */` block ending on line `end`, stripping the comment
/// markers and leading ` * ` continuations. Returns `None` for a plain `/* */`
/// comment (only `/**` counts as documentation), and for a block that opens
/// `MAX_BLOCK_LINES` or more lines above `end` (i.e. one spanning more than
/// that many lines — the const's own doc states the same bound that way).
fn collect_block(lines: &[&str], openers: &[Option<usize>], end: usize) -> Option<String> {
    // `openers` must be the index built for THESE lines. Routing another style
    // here without building it would not look broken — every lookup would miss
    // and the file would simply report no documentation — so say so loudly.
    debug_assert_eq!(openers.len(), lines.len(), "block index not built");
    if !lines[end].contains("*/") {
        return None;
    }
    let start = openers.get(end).copied().flatten()?;
    if end - start >= MAX_BLOCK_LINES {
        return None;
    }
    if !lines[start].contains("/**") {
        return None;
    }
    work::add(end - start + 1);
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

/// Clean documentation markup that isn't Markdown so it renders as prose rather
/// than literal noise — each language's own markup only:
///   Dart    `{@template}` / `{@endtemplate}` / `{@macro}` directive markers
///           are dropped (the text between them kept);
///   Javadoc / JSDoc / Doxygen inline tags keep their CONTENT: `{@code x}`
///           becomes `` `x` ``, `{@link Foo label}` becomes `label` (or `Foo`);
///   Python  reST cross-reference roles (``:class:`Foo``` → `Foo`, honoring a
///           leading `~`/`.`).
///
/// Deleting every `{@…}` in every language — what this used to do — erased
/// the very words Javadoc wraps in `{@code …}` and `{@link …}`, leaving
/// sentences like "Returns  if the key is present".
fn clean_doc_text(s: &str, lang: Lang) -> String {
    match lang {
        Lang::Dart => strip_dart_directives(s),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx | Lang::Java | Lang::C | Lang::Cpp => {
            render_inline_tags(s)
        }
        Lang::Python => simplify_rest_roles(s),
        Lang::Rust
        | Lang::Go
        | Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
        | Lang::Zig => s.to_string(),
    }
}

/// Drop dartdoc `{@ … }` directive markers, keeping the text around them.
fn strip_dart_directives(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find("{@") {
        out.push_str(&rest[..pos]);
        match rest[pos..].find('}') {
            Some(end) => rest = &rest[pos + end + 1..],
            None => {
                rest = &rest[pos + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Render Javadoc/JSDoc inline tags as the text they stand for.
fn render_inline_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("{@") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        // Braces nest inside a tag (`{@code Map<K, {V}>}`).
        let Some(close) = matching_brace(after) else {
            // Unterminated: leave the rest as written.
            out.push_str(&rest[open..]);
            return out;
        };
        let inner = &after[..close];
        let (tag, body) = match inner.split_once(char::is_whitespace) {
            Some((tag, body)) => (tag, body.trim()),
            None => (inner, ""),
        };
        match tag {
            "code" => out.push_str(&inline_code(body)),
            "link" | "linkplain" | "linkcode" => out.push_str(&link_label(body)),
            // Pure directives: nothing to show in their place.
            "inheritDoc" | "inheritdoc" | "docRoot" => {}
            // `{@literal x}`, `{@value X}`, and anything unknown: the content.
            _ => out.push_str(body),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The index of the `}` closing a tag whose `{` precedes `s`.
fn matching_brace(s: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `x` as Markdown inline code, fenced so a backtick inside it cannot close it.
fn inline_code(x: &str) -> String {
    if x.is_empty() {
        return String::new();
    }
    if x.contains('`') {
        format!("`` {x} ``")
    } else {
        format!("`{x}`")
    }
}

/// What a link tag shows: its label (`{@link Foo the foo}`, JSDoc
/// `{@link Foo|the foo}`), else its target with the member separator made
/// readable (`#bar` → `bar`, `Foo#bar` → `Foo.bar`).
fn link_label(body: &str) -> String {
    if let Some((_, label)) = body.split_once('|') {
        return label.trim().to_string();
    }
    if let Some((_, label)) = body.split_once(char::is_whitespace) {
        let label = label.trim();
        if !label.is_empty() {
            return label.to_string();
        }
    }
    let target = body.strip_prefix('#').unwrap_or(body);
    target.replace('#', ".")
}

/// Simplify reST roles `:role:`target`` → target.
fn simplify_rest_roles(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < s.len() {
        if bytes[i] == b':'
            && let Some((shown, consumed)) = rest_role_at(&s[i..])
        {
            out.push_str(&shown);
            i += consumed;
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    out
}

/// Longest role name (`py:class`, `meth`, …) and target a reST role may have.
/// Both bound the scan made at every `:` in a doc, which used to search the
/// whole rest of the text each time — quadratic on a long docstring full of
/// colons.
const MAX_ROLE_NAME: usize = 32;
const MAX_ROLE_TARGET: usize = 256;

/// If `s` starts with a reST cross-reference role ``:name:`target```, return the
/// simplified target text and the byte length consumed.
fn rest_role_at(s: &str) -> Option<(String, usize)> {
    let after_first = s.get(1..)?; // past the leading ':'
    // The role name ends at the ':' immediately before the opening backtick, so a
    // domain-qualified role like `py:class` (with an internal colon) is captured.
    let mut close = None;
    for (i, c) in after_first.char_indices().take(MAX_ROLE_NAME + 1) {
        work::add(1);
        if c == ':' && after_first[i + 1..].starts_with('`') {
            close = Some(i);
            break;
        }
        if !(c.is_ascii_alphabetic() || c == ':') {
            return None;
        }
    }
    let close = close.filter(|&c| c > 0)?;
    let target_part = &after_first[close + 2..]; // past ":`"
    let target_end = target_part
        .char_indices()
        .take(MAX_ROLE_TARGET)
        .inspect(|_| work::add(1))
        .find(|&(_, c)| c == '`')
        .map(|(i, _)| i)?;
    let target = &target_part[..target_end];
    let shown = if let Some(t) = target.strip_prefix('~') {
        t.rsplit(['.', ':']).next().unwrap_or(t)
    } else {
        target.strip_prefix('.').unwrap_or(target)
    };
    // leading ':' (1) + name + ":`" (2) + target + closing '`' (1)
    Some((shown.to_string(), 1 + close + 2 + target_end + 1))
}

/// Most lines a def/class header may span. A real one, however wrapped, is a
/// handful; the bound keeps a symbol whose header never closes from walking
/// the rest of the file.
const MAX_PY_HEADER_LINES: usize = 64;

/// The 0-based index of the first body line of a `def`/`class` whose header
/// begins at `def_idx`. Walks the (possibly multi-line) signature until brackets
/// are balanced and a line ends with the body-opening `:`, then returns the next
/// line — so a wrapped, fully-typed signature doesn't hide its docstring.
/// `None` when `def_idx` does not open a def/class header, and for a one-line
/// def whose body follows its colon (`def stub(): ...`): brackets balanced on
/// a line that does not end with `:` (or a `\` continuation) is the end of
/// the statement, and walking on used to take the NEXT def's header for this
/// one's and hand this stub that function's docstring.
fn py_body_start(lines: &[&str], def_idx: usize) -> Option<usize> {
    let head = lines.get(def_idx)?.trim_start();
    if !(head.starts_with("def ") || head.starts_with("async def ") || head.starts_with("class ")) {
        return None;
    }
    let mut depth: i32 = 0;
    for (i, line) in lines
        .iter()
        .enumerate()
        .skip(def_idx)
        .take(MAX_PY_HEADER_LINES)
    {
        work::add(1);
        // Brackets, colons and `#` inside a string default (`x="(#"`) are
        // not the header's.
        let blanked = crate::highlight::without_string_literals(line, &['"', '\'']);
        // Ignore a trailing line comment before checking the colon.
        let code = blanked.split('#').next().unwrap_or(&blanked).trim_end();
        for ch in code.chars() {
            match ch {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                _ => {}
            }
        }
        if depth <= 0 {
            if code.ends_with(':') {
                return Some(i + 1);
            }
            if !code.ends_with('\\') {
                return None;
            }
        }
    }
    None
}

fn py_docstring(lines: &[&str], sig_line: usize) -> Option<String> {
    // `sig_line` is the header's first line; for a multi-line signature the
    // body — and thus the docstring — is further down, so resolve the real body
    // start from the header line.
    let def_idx = sig_line.saturating_sub(1);
    let mut i = py_body_start(lines, def_idx)?;
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
                work::add(1);
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
///
/// The indent is measured and cut in CHARS, never bytes: `trim_start` strips
/// every Unicode whitespace char, so a line indented with U+00A0 or U+3000 has
/// a byte-indent that lands mid-character in a sibling line indented with
/// plain spaces, and slicing there panics. That panic happens inside the
/// worker that builds a file's docs, so the whole file silently loses its
/// content (no FileContent reply) over one pasted non-breaking space.
/// Counting chars keeps the ASCII behaviour identical and can only ever cut
/// inside the leading whitespace run, never inside the text.
fn dedent(lines: &[String]) -> String {
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| match l.char_indices().nth(min_indent) {
            Some((b, _)) => &l[b..],
            // Shorter than the common indent, so it is blank by construction:
            // every non-blank line has at least `min_indent` chars.
            None => "",
        })
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
        assert_eq!(
            clean_doc_text("{@template x}Hello{@endtemplate}", Lang::Dart),
            "Hello"
        );
        assert_eq!(
            clean_doc_text("See {@macro foo} here", Lang::Dart),
            "See  here"
        );
        // reST roles simplified to their target, honoring ~ and leading dot.
        assert_eq!(
            clean_doc_text("A :class:`Request` object", Lang::Python),
            "A Request object"
        );
        assert_eq!(
            clean_doc_text("uses :py:class:`Foo`", Lang::Python),
            "uses Foo"
        );
        assert_eq!(
            clean_doc_text("call :meth:`~scrapy.Request.replace`", Lang::Python),
            "call replace"
        );
        // Plain colons (not a role) are untouched.
        assert_eq!(
            clean_doc_text("note: this is fine", Lang::Python),
            "note: this is fine"
        );
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
        let d = docs
            .values()
            .next()
            .expect("docstring for the multi-line def");
        assert!(d.contains("Sends an HTTP request"), "{d}");
    }

    #[test]
    fn python_docstring_indented_with_a_multibyte_space() {
        // An ideographic space used as indentation (a paste artifact, but a real
        // one) used to make the byte-measured dedent slice mid-character and
        // panic, which killed the whole file's extraction, not just this doc.
        let src =
            "def f():\n    \"\"\"\n    Hello\n\u{3000}\u{3000}World\n    \"\"\"\n    return 1\n";
        let docs = docs_of(src, "python");
        let d = docs.values().next().expect("a docstring");
        assert!(d.contains("Hello") && d.contains("World"), "{d:?}");
        // The ideographic spaces are indentation, so they are cut, not kept.
        assert!(!d.contains('\u{3000}'), "{d:?}");
    }

    #[test]
    fn python_docstring_dedent_cuts_before_multibyte_text() {
        // NBSP indent on one line, plain spaces on another: the common indent
        // (2 chars) ends exactly where multi-byte text begins, the case a
        // byte-measured cut split in half.
        let src = "def g():\n    \"\"\"\n  Hi\n \u{a0}中文注释\n    \"\"\"\n    return 2\n";
        let docs = docs_of(src, "python");
        let d = docs.values().next().expect("a docstring");
        assert_eq!(d, "Hi\n中文注释");
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
    fn overloaded_function_impl_inherits_the_overload_jsdoc() {
        // The JSDoc sits above the first overload signature; the implementation
        // below shares the name but no doc of its own — it should inherit.
        let src = "/**\n * Makes a thing.\n */\n\
                   export function make<T>(x: T): T;\n\
                   export function make(x: unknown) { return x }\n";
        let docs = docs_of(src, "typescript");
        let with_doc = docs
            .values()
            .filter(|d| d.contains("Makes a thing"))
            .count();
        assert!(with_doc >= 2, "impl did not inherit overload doc: {docs:?}");
    }

    #[test]
    fn doc_does_not_leak_between_unrelated_same_named_methods() {
        // `get` in two classes are same-named but never adjacent (the second
        // class sits between them), so B's getter must not inherit A's doc.
        let src = "class A {\n  /** A's getter. */\n  get(): number { return 1 }\n}\n\
                   class B {\n  get(): number { return 2 }\n}\n";
        let docs = docs_of(src, "typescript");
        let count = docs.values().filter(|d| d.contains("A's getter")).count();
        assert_eq!(count, 1, "doc leaked to B.get: {docs:?}");
    }

    #[test]
    fn jsdoc_survives_a_side_effect_pragma_above_the_signature() {
        // Vue/Rollup put `/*@__NO_SIDE_EFFECTS__*/` directly above exported
        // functions; it must not mask the JSDoc above it (was "No documentation").
        let src = "/**\n * Returns a reactive proxy of the object.\n */\n\
                   /*@__NO_SIDE_EFFECTS__*/\n\
                   export function reactive(target) { return target }\n";
        let docs = docs_of(src, "typescript");
        let all: Vec<&str> = docs.values().map(String::as_str).collect();
        assert!(
            all.iter().any(|d| d.contains("Returns a reactive proxy")),
            "pragma masked the JSDoc: {docs:?}"
        );
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
            all.iter()
                .any(|d| d.contains("Sets the foreground to an RGB color")),
            "member JSDoc not extracted: {docs:?}"
        );
    }

    /// One sample per documented language, deliberately including the shapes
    /// the block-comment index has to reproduce: a JSDoc above a pragma, a
    /// plain `/* … */` that is NOT a doc, a `*/` inside a string literal with
    /// no comment anywhere above it, and two block comments in one file so the
    /// NEAREST opener is the one that must win.
    fn multi_language_sample() -> Vec<(&'static str, &'static str)> {
        vec![
            (
                "rust",
                "//! Module docs.\n\n/// Adds.\n#[inline]\npub fn add(a: i32) -> i32 { a }\n\n// not a doc\npub fn bare() {}\n",
            ),
            (
                "go",
                "// Add returns the sum.\nfunc Add(a, b int) int {\n\treturn a + b\n}\n\nfunc Bare() {}\n",
            ),
            (
                "javascript",
                "/**\n * First block.\n */\nfunction one() {}\n\n/* plain, not a doc */\nfunction two() {}\n\n/**\n * Second block.\n */\n/*@__PURE__*/\nfunction three() {}\n\nconst s = \"*/\";\nfunction four() {}\n",
            ),
            (
                "typescript",
                "/**\n * Makes a thing.\n */\nexport function make<T>(x: T): T;\nexport function make(x: unknown) { return x }\n\nclass A {\n  /** A's getter. */\n  get(): number { return 1 }\n}\nclass B {\n  get(): number { return 2 }\n}\n",
            ),
            (
                "java",
                "/**\n * A widget.\n */\npublic class Widget {\n  /** Renders it. */\n  @Override\n  public void render() {}\n  public void undocumented() {}\n}\n",
            ),
            (
                "c",
                "/**\n * Doubles n.\n */\nint dbl(int n) { return n * 2; }\n\n/* internal */\nstatic int helper(void) { return 0; }\n",
            ),
            (
                "cpp",
                "/**\n * A shape.\n */\nclass Shape {\n  /** The area. */\n  double area() const;\npublic:\n  /** The name. */\n  const char *name() const;\n};\n",
            ),
            (
                "python",
                "def greet(name):\n    \"\"\"Say hello.\"\"\"\n    return name\n\ndef quiet():\n    return 1\n\nclass K:\n    '''A class.'''\n    def m(self):\n        \"\"\"A method.\"\"\"\n",
            ),
            (
                "dart",
                "/// A parser.\nclass ArgParser {\n  /// Adds a flag.\n  @Deprecated('x')\n  void addFlag(String name) {}\n}\n",
            ),
        ]
    }

    /// Pinned against the output of the pre-index implementation, captured
    /// before the block-comment scan was replaced. The rewrite is a pure
    /// performance change, so every one of these must stay byte-for-byte.
    #[test]
    fn the_multi_language_sample_extracts_exactly_what_it_always_did() {
        let golden: Vec<(&str, Vec<(usize, &str)>)> = vec![
            ("rust", vec![(5, "Adds.")]),
            ("go", vec![(2, "Add returns the sum.")]),
            // `four` is the interesting one: its `*/` is inside a string, and
            // the NEAREST opener above it is the `/*@__PURE__*/` pragma, which
            // is not a doc. It must stay undocumented rather than reaching past
            // the pragma to the "Second block." JSDoc.
            (
                "javascript",
                vec![(4, "First block."), (13, "Second block.")],
            ),
            (
                "typescript",
                vec![
                    (4, "Makes a thing."),
                    (5, "Makes a thing."),
                    (9, "A's getter."),
                ],
            ),
            ("java", vec![(4, "A widget."), (6, "Renders it.")]),
            ("c", vec![(4, "Doubles n.")]),
            (
                "cpp",
                vec![(4, "A shape."), (6, "The area."), (9, "The name.")],
            ),
            (
                "python",
                vec![(1, "Say hello."), (8, "A class."), (10, "A method.")],
            ),
            ("dart", vec![(2, "A parser."), (5, "Adds a flag.")]),
        ];
        for ((lang, src), (glang, want)) in multi_language_sample().into_iter().zip(golden) {
            assert_eq!(lang, glang, "sample and golden are out of step");
            let mut got: Vec<(usize, String)> = docs_of(src, lang).into_iter().collect();
            got.sort();
            let want: Vec<(usize, String)> =
                want.into_iter().map(|(l, d)| (l, d.to_string())).collect();
            assert_eq!(got, want, "{lang} docs changed");
        }
    }

    /// The shape that made opening a file hang: every symbol is preceded by a
    /// line that CONTAINS `*/` (a legal string literal) while the file holds no
    /// `/*` at all, so the old upward scan ran from each symbol to line 0 —
    /// O(symbols × lines). 20k symbols over 40k lines is 400M line comparisons
    /// on the old path (12.9 s in a debug build); the index answers each in
    /// O(1). Asserted on the lines visited, not on a wall clock.
    ///
    /// The outline is built by hand rather than parsed, so this measures doc
    /// extraction and not tree-sitter.
    #[test]
    fn a_string_holding_a_comment_close_does_not_rescan_the_file_per_symbol() {
        const N: usize = 20_000;
        let mut src = String::new();
        let mut symbols = Vec::new();
        for i in 0..N {
            src.push_str("const s = \"*/\";\n");
            src.push_str("function f() {}\n");
            symbols.push(Symbol {
                name: format!("f{i}"),
                kind: "function".into(),
                line: 2 * i + 2,
                end_line: 2 * i + 2,
            });
        }
        let (docs, visited) = work::measure(|| extract(&src, "javascript", &symbols));
        // No `/*` anywhere, so nothing here is documentation.
        assert!(docs.is_empty(), "{} spurious docs", docs.len());
        // One pass over the lines, plus O(1) per symbol.
        let bound = 2 * N + 4 * N;
        assert!(
            visited <= bound,
            "visited {visited} lines for {N} symbols (bound {bound}): a per-symbol rescan"
        );
    }

    /// A doc block is local to its symbol. With one `/**` at the top of a file
    /// and thousands of `*/` lines below it, every symbol's nearest opener is
    /// that single line, so without the distance limit each symbol joins nearly
    /// the whole file into its own owned doc string — 23.0 s and 5000 file-sized
    /// docs in a debug build, quadratic in memory as well as time, and none of
    /// it is really that symbol's documentation. Asserted on the lines joined.
    #[test]
    fn a_comment_opener_far_above_a_symbol_is_not_its_doc() {
        const N: usize = 5_000;
        let mut src = String::from("/**\n * Opened here and never closed.\n");
        let mut symbols = Vec::new();
        for i in 0..N {
            src.push_str("const s = \"*/\";\n");
            src.push_str("function f() {}\n");
            symbols.push(Symbol {
                name: format!("f{i}"),
                kind: "function".into(),
                line: 2 * i + 4,
                end_line: 2 * i + 4,
            });
        }
        let (docs, visited) = work::measure(|| extract(&src, "javascript", &symbols));
        // Only the symbols within MAX_BLOCK_LINES of the opener can see it.
        assert!(
            docs.len() < MAX_BLOCK_LINES,
            "{} symbols reached a block {N} lines above them",
            docs.len()
        );
        assert!(
            docs.contains_key(&4),
            "the symbol right below it keeps its doc"
        );
        assert!(
            !docs.contains_key(&(2 * N + 2)),
            "the last symbol claimed a doc opened {N} lines above it"
        );
        // The index pass, O(1) per symbol, and at most MAX_BLOCK_LINES joined
        // for each of the symbols close enough to the opener to see it. The
        // unbounded join was ~N²/2 lines (12.5M here).
        let bound = 2 * N + 2 + 4 * N + MAX_BLOCK_LINES * MAX_BLOCK_LINES;
        assert!(
            visited <= bound,
            "visited {visited} lines (bound {bound}): a whole-file join per symbol"
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
        assert!(
            all.iter()
                .any(|d| d.contains("A parser for command-line arguments")),
            "class doc: {docs:?}"
        );
        assert!(
            all.iter().any(|d| d.contains("Adds a boolean flag")),
            "method doc above @annotation: {docs:?}"
        );
    }
}

#[cfg(test)]
mod markup_tests {
    use super::*;

    fn docs_of(src: &str, lang: &str) -> HashMap<usize, String> {
        let syms = crate::outline::extract(src, lang);
        extract(src, lang, &syms)
    }

    /// Javadoc and JSDoc inline tags keep their words. Deleting every `{@…}`
    /// turned "Returns {@code true} if …" into "Returns  if …".
    #[test]
    fn javadoc_and_jsdoc_inline_tags_keep_their_content() {
        let render = |s: &str| clean_doc_text(s, Lang::Java);
        assert_eq!(
            render("Returns {@code true} if set."),
            "Returns `true` if set."
        );
        assert_eq!(render("See {@link Foo}."), "See Foo.");
        assert_eq!(render("See {@link Foo#bar(int) the bar}."), "See the bar.");
        assert_eq!(render("Calls {@link #reset}."), "Calls reset.");
        assert_eq!(render("Uses {@link Map#get}."), "Uses Map.get.");
        assert_eq!(
            render("A {@code Map<K, {V}>} value"),
            "A `Map<K, {V}>` value"
        );
        assert_eq!(render("{@inheritDoc} More."), " More.");
        assert_eq!(render("Is {@literal <b>} raw"), "Is <b> raw");
        assert_eq!(render("broken {@code x"), "broken {@code x");
        assert_eq!(
            clean_doc_text("Opens {@link Door|the door}.", Lang::TypeScript),
            "Opens the door."
        );
        assert_eq!(clean_doc_text("Has `{@code}`", Lang::Java), "Has ``");

        // End to end through a real Javadoc block.
        let src = "class A {\n  /**\n   * Returns {@code true} when {@link B} is ready.\n   */\n  boolean ready() { return true; }\n}\n";
        let docs = docs_of(src, "java");
        assert!(
            docs.values()
                .any(|d| d == "Returns `true` when B is ready."),
            "{docs:?}"
        );
    }

    /// Each language's markup is cleaned for that language only: a Rust doc
    /// that mentions `{@` or a reST-looking role is left exactly as written.
    #[test]
    fn markup_rules_apply_to_their_own_language_only() {
        let s = "Parses `{@code}` and :class:`Foo` literally.";
        assert_eq!(clean_doc_text(s, Lang::Rust), s);
        assert_eq!(clean_doc_text(s, Lang::Go), s);
        assert_eq!(
            clean_doc_text("Limit is {@value MAX}.", Lang::Java),
            "Limit is MAX.",
            "other Javadoc tags keep their content"
        );
        assert_eq!(
            clean_doc_text("{@template t}Body{@endtemplate}", Lang::Dart),
            "Body"
        );
    }

    /// Only a def or class has a docstring. A module constant used to reach
    /// forward to the next `def` and take ITS docstring.
    #[test]
    fn python_constants_do_not_borrow_the_next_functions_docstring() {
        let src = "VERSION = \"1.0\"\nNAME = 'x'\n\ndef main():\n    \"\"\"Entry point.\"\"\"\n    return 0\n";
        let docs = docs_of(src, "python");
        assert_eq!(docs.get(&4).map(String::as_str), Some("Entry point."));
        assert!(!docs.contains_key(&1), "VERSION got a doc: {docs:?}");
        assert!(!docs.contains_key(&2), "NAME got a doc: {docs:?}");
    }

    /// Many constants before one function: every constant used to scan to that
    /// function — O(constants × lines), 200M lines here. The outline is built
    /// by hand so this measures doc extraction alone, and the lines it visits
    /// are counted rather than timed.
    #[test]
    fn python_constants_cost_nothing() {
        const N: usize = 20_000;
        let mut src = String::new();
        let mut symbols = Vec::new();
        for i in 0..N {
            src.push_str(&format!("C{i} = {i}\n"));
            symbols.push(Symbol {
                name: format!("C{i}"),
                kind: "constant".into(),
                line: i + 1,
                end_line: i + 1,
            });
        }
        src.push_str("def f():\n    \"\"\"Doc.\"\"\"\n");
        symbols.push(Symbol {
            name: "f".into(),
            kind: "function".into(),
            line: N + 1,
            end_line: N + 2,
        });
        let (docs, visited) = work::measure(|| extract(&src, "python", &symbols));
        assert_eq!(
            docs.len(),
            1,
            "{:?}",
            docs.keys().take(5).collect::<Vec<_>>()
        );
        // The def's own header and docstring, nothing per constant.
        assert!(
            visited <= 8,
            "visited {visited} lines for one docstring: constants are being scanned"
        );
    }

    /// Two `new`s in adjacent impl blocks are two types' constructors, not
    /// overloads — Rust has no overloading, so no doc is shared.
    #[test]
    fn same_name_methods_of_different_types_do_not_share_docs() {
        let rust = "struct A;\nstruct B;\nimpl A {\n    /// Makes an A.\n    fn new() -> A { A }\n}\nimpl B {\n    fn new() -> B { B }\n}\n";
        let docs = docs_of(rust, "rust");
        assert_eq!(docs.get(&5).map(String::as_str), Some("Makes an A."));
        assert!(
            !docs.contains_key(&8),
            "B::new inherited A::new's doc: {docs:?}"
        );

        let go = "package p\n\n// String names an A.\nfunc (a A) String() string { return \"a\" }\nfunc (b B) String() string { return \"b\" }\n";
        let docs = docs_of(go, "go");
        assert_eq!(docs.len(), 1, "{docs:?}");

        // Languages WITH overloading still share (see the TypeScript test).
        let java = "class A {\n  /** Adds. */\n  int add(int a) { return a; }\n  int add(int a, int b) { return a + b; }\n}\n";
        let docs = docs_of(java, "java");
        assert_eq!(
            docs.values().filter(|d| *d == "Adds.").count(),
            2,
            "{docs:?}"
        );
    }

    /// A one-line def has no docstring of its own; walking past its header
    /// used to take the NEXT def's for it. A string default holding brackets
    /// or a `#` does not unbalance or cut the header.
    #[test]
    fn a_python_one_liner_does_not_take_the_next_functions_docstring() {
        let src = "def stub(): ...\ndef real():\n    \"\"\"Real doc.\"\"\"\n    return 1\n\
                   def tricky(x=\"(#\", y=')'):\n    \"\"\"Tricky doc.\"\"\"\n\
                   def cont(a, b) \\\n        -> int:\n    \"\"\"Continued.\"\"\"\n";
        let docs = docs_of(src, "python");
        assert!(!docs.contains_key(&1), "the stub took a doc: {docs:?}");
        assert_eq!(docs.get(&2).map(String::as_str), Some("Real doc."));
        assert_eq!(docs.get(&5).map(String::as_str), Some("Tricky doc."));
        assert_eq!(docs.get(&7).map(String::as_str), Some("Continued."));
    }

    /// Same-name NEIGHBOURS of different owners are not overloads: C++
    /// `A::draw` right after `B::draw`, or the `run` entries of two object
    /// literals, each keep their own doc (or none). Both used to share, since
    /// the name and adjacency were all the grouping looked at.
    #[test]
    fn a_doc_is_shared_only_among_members_of_one_owner() {
        let cpp = "struct A { void draw(); };\nstruct B { void draw(); };\n\
                   /** Draws an A. */\nvoid A::draw() {}\nvoid B::draw() {}\n";
        let docs = docs_of(cpp, "cpp");
        assert_eq!(docs.get(&4).map(String::as_str), Some("Draws an A."));
        assert!(
            !docs.contains_key(&5),
            "B::draw took A::draw's doc: {docs:?}"
        );

        let ts = "const a = {\n  /** Runs a. */\n  run: () => 1,\n};\nconst b = {\n  run: () => 2,\n};\n";
        let docs = docs_of(ts, "typescript");
        assert_eq!(docs.get(&3).map(String::as_str), Some("Runs a."));
        assert!(!docs.contains_key(&6), "b.run took a.run's doc: {docs:?}");

        // Written one per line, the two literals' entries are adjacent lines.
        let ts = "/** Handlers. */\nconst a = { run: () => 1 };\nconst b = { run: () => 2 };\n";
        let docs = docs_of(ts, "typescript");
        assert!(docs.contains_key(&2), "{docs:?}");
        assert!(!docs.contains_key(&3), "b.run took a.run's doc: {docs:?}");

        // The located path (the Docs view) groups the same way.
        let items = crate::outline::extract_located(cpp, "cpp");
        let docs = extract_located(cpp, "cpp", &items);
        assert!(!docs.contains_key(&5), "{docs:?}");
    }

    /// Real overload sets still share their doc — declared side by side in
    /// one scope, under one qualifier.
    #[test]
    fn overloads_in_one_owner_still_share_their_doc() {
        let shared = |src: &str, lang: &str, lines: &[usize], doc: &str| {
            let docs = docs_of(src, lang);
            for line in lines {
                assert_eq!(
                    docs.get(line).map(String::as_str),
                    Some(doc),
                    "{lang} line {line}: {docs:?}"
                );
            }
        };
        shared(
            "class A {\n  /** Draws. */\n  draw(x: number): void;\n  draw(x: any) {}\n}\n",
            "typescript",
            &[3, 4],
            "Draws.",
        );
        shared(
            "struct S {\n  /** Adds. */\n  int add(int a);\n  int add(int a, int b);\n};\n",
            "cpp",
            &[3, 4],
            "Adds.",
        );
        shared(
            "/** Adds. */\nint S::add(int a) { return a; }\nint S::add(int a, int b) { return a + b; }\n",
            "cpp",
            &[2, 3],
            "Adds.",
        );
        // A member declared in its class and defined right after it is one
        // function written twice; so is a namespace member.
        shared(
            "struct S {\n  /** Adds. */\n  int add(int a);\n};\ninline int S::add(int a) { return a; }\n",
            "cpp",
            &[3, 5],
            "Adds.",
        );
        shared(
            "namespace geo {\n/** Area. */\ndouble area(double r);\n}\ndouble geo::area(double r) { return r; }\n",
            "cpp",
            &[3, 5],
            "Area.",
        );
        shared(
            "class K:\n    @overload\n    def f(self, x: int) -> int:\n        \"\"\"Converts.\"\"\"\n    @overload\n    def f(self, x: str) -> str: ...\n    def f(self, x):\n        return x\n",
            "python",
            &[3, 6, 7],
            "Converts.",
        );
    }

    /// A GNU-style C definition keeps its doc above the return-type line.
    #[test]
    fn a_located_outline_finds_docs_above_the_declaration_start() {
        let src = "/** Opens the thing. */\nstatic int\nopen_thing(void)\n{\n  return 0;\n}\n";
        let items = crate::outline::extract_located(src, "c");
        let docs = extract_located(src, "c", &items);
        assert_eq!(
            docs.get(&3).map(String::as_str),
            Some("Opens the thing."),
            "{docs:?}"
        );
        // The name-line anchor alone misses it.
        assert!(docs_of(src, "c").is_empty());
    }

    /// The role scan is bounded: a long docstring full of colons stays linear
    /// — at most a role name's and a target's length per colon, where the
    /// unbounded scan read the rest of the text at every one (4·10¹⁰ chars
    /// here). Counted, not timed.
    #[test]
    fn rest_roles_scan_is_bounded() {
        const COLONS: usize = 200_000;
        let s = ":a".repeat(COLONS);
        let (out, scanned) = work::measure(|| clean_doc_text(&s, Lang::Python));
        assert_eq!(out, s);
        let bound = COLONS * (MAX_ROLE_NAME + 1 + MAX_ROLE_TARGET);
        assert!(scanned <= bound, "scanned {scanned} chars (bound {bound})");
        assert_eq!(rest_role_at(":x:`y`"), Some(("y".to_string(), 6)));
        assert_eq!(rest_role_at("::`y`"), None);
    }
}
