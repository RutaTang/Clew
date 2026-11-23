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
//!   Lean    a `/-- … -/` block above
//!   SQL     the run of `--` lines immediately above a `CREATE`

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
    extract_with(source, lang_key, located_targets(items), usize::MAX)
}

/// A file's outline and the hover peek's docs for it, from one parse: each
/// doc looked for above the line its declaration starts on, as the Docs view
/// does, and kept to `MAX_DOC`. Looked for above the name's line ([`extract`]),
/// the doc of a C++ template, a GNU-style C definition, a JS `export` on a
/// line of its own and a TS class under a decorator was never found.
pub fn outline_and_peek(source: &str, lang_key: &str) -> (Vec<Symbol>, HashMap<usize, String>) {
    let items = crate::outline::extract_located(source, lang_key);
    let docs = extract_with(source, lang_key, located_targets(&items), MAX_DOC);
    (items.into_iter().map(|l| l.symbol).collect(), docs)
}

fn located_targets(items: &[Located]) -> Vec<Target<'_>> {
    items
        .iter()
        .map(|l| Target {
            key: l.symbol.line,
            anchor: l.decl_line,
            kind: &l.symbol.kind,
            name: &l.symbol.name,
            owner: Some(l.owner.clone()),
        })
        .collect()
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
        DocStyle::Block(syntax) => block_openers(&lines, syntax.open),
        _ => Vec::new(),
    };
    // Every style that reads above a signature consults this one.
    let ends = match style {
        DocStyle::Block(_)
        | DocStyle::RustLike
        | DocStyle::SlashSlash
        | DocStyle::DashDash
        | DocStyle::Hash
        | DocStyle::Percent
        | DocStyle::Haddock => doc_ends(&lines, lang),
        DocStyle::PyDocstring | DocStyle::None => Vec::new(),
    };
    let mut out = HashMap::new();
    for t in &targets {
        if out.contains_key(&t.key) {
            continue;
        }
        if let Some(mut doc) = style.doc_for(&lines, &openers, &ends, t) {
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
        Lang::TypeScript | Lang::Tsx | Lang::Java | Lang::Cpp | Lang::Python | Lang::CSharp => true,
        Lang::Php | Lang::Ruby | Lang::Erlang | Lang::Haskell => false,
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
        | Lang::Zig
        | Lang::Sql
        | Lang::Lean => false,
    }
}

/// A block-comment doc syntax.
struct BlockSyntax {
    /// What opens a doc (`/**`).
    doc: &'static str,
    /// What opens any block comment (`/*`), a doc or not.
    open: &'static str,
    /// What closes one (`*/`).
    close: &'static str,
    /// Whether continuation lines carry a ` * ` gutter to strip. Lean's do
    /// not, and a `*` starting one of its lines is a Markdown bullet.
    gutter: bool,
}

/// JSDoc, Javadoc and Doxygen: `/** … */`.
const C_BLOCK: BlockSyntax = BlockSyntax {
    doc: "/**",
    open: "/*",
    close: "*/",
    gutter: true,
};

/// Lean: `/-- … -/`. A `/-! … -/` block documents the module it sits in.
const LEAN_BLOCK: BlockSyntax = BlockSyntax {
    doc: "/--",
    open: "/-",
    close: "-/",
    gutter: false,
};

enum DocStyle {
    /// Rust: `///` and `//!` line docs above the item.
    RustLike,
    /// Go: the run of `//` lines immediately above the declaration.
    SlashSlash,
    /// SQL: the run of `--` lines immediately above the statement.
    DashDash,
    /// Ruby: the run of `#` lines immediately above the definition.
    Hash,
    /// Erlang's EDoc: the run of `%` lines above (`%% @doc …`).
    Percent,
    /// Haskell's Haddock: a run of `--` lines above whose first is `-- |`.
    Haddock,
    /// JSDoc / Doxygen (`/** … */`), Lean (`/-- … -/`): a block above.
    Block(&'static BlockSyntax),
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
            Lang::Rust | Lang::Dart | Lang::CSharp | Lang::Zig => DocStyle::RustLike,
            Lang::Php => DocStyle::Block(&C_BLOCK),
            Lang::Ruby => DocStyle::Hash,
            Lang::Erlang => DocStyle::Percent,
            Lang::Haskell => DocStyle::Haddock,
            Lang::Go => DocStyle::SlashSlash,
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx | Lang::Java | Lang::C | Lang::Cpp => {
                DocStyle::Block(&C_BLOCK)
            }
            Lang::Lean => DocStyle::Block(&LEAN_BLOCK),
            Lang::Python => DocStyle::PyDocstring,
            Lang::Sql => DocStyle::DashDash,
            Lang::Json | Lang::Bash | Lang::Yaml | Lang::Toml | Lang::Html | Lang::Css => {
                DocStyle::None
            }
        }
    }

    fn doc_for(
        &self,
        lines: &[&str],
        openers: &[Option<usize>],
        ends: &[Option<usize>],
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
            _ => self.above_doc(lines, openers, ends, target.anchor),
        }
    }

    /// Docs that sit on the lines above a signature (every style but Python).
    /// `openers` is the block-comment index, empty for the styles that never
    /// look at it; `ends` is [`doc_ends`].
    fn above_doc(
        &self,
        lines: &[&str],
        openers: &[Option<usize>],
        ends: &[Option<usize>],
        sig_line: usize,
    ) -> Option<String> {
        if sig_line < 2 || sig_line > lines.len() + 1 {
            return None;
        }
        // Up from the line directly above the signature, past the lines glued
        // between it and its doc.
        let end = ends.get(sig_line - 2).copied().flatten()?;
        match self {
            DocStyle::Block(syntax) => collect_block(lines, openers, end, syntax),
            // `//!` documents the module it sits in, never the item below it.
            DocStyle::RustLike => collect_line_docs(lines, end, &["///"]),
            DocStyle::SlashSlash => collect_line_docs(lines, end, &["//"]),
            DocStyle::DashDash => collect_line_docs(lines, end, &["--"]),
            // A magic comment (`# frozen_string_literal: true`) or a
            // shebang is no doc.
            DocStyle::Hash => collect_line_docs(lines, end, &["#"])
                .filter(|d| !d.starts_with('!') && !d.contains("frozen_string_literal:")),
            DocStyle::Percent => collect_line_docs(lines, end, &["%%%", "%%", "%"]),
            DocStyle::Haddock => {
                let doc = collect_line_docs(lines, end, &["--"])?;
                let doc = doc.strip_prefix('|')?;
                non_empty(doc.trim_start())
            }
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
        // `////` is a plain comment, as rustdoc and dartdoc read it.
        if *prefix == "///" && t.starts_with("////") {
            break;
        }
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

/// For every line, the nearest line at or above it that a doc for the
/// signature below may end on, or `None` when there is none: the lines glued
/// between a doc and its signature are passed over — attribute and
/// annotation lines (Rust `#[derive]`, Java `@Override`) and single-line
/// non-doc block-comment pragmas (Rollup's `/*@__NO_SIDE_EFFECTS__*/`,
/// `/*#__PURE__*/`, ubiquitous above exported functions in JS/TS libraries,
/// where they masked the real JSDoc above them).
///
/// An attribute or decorator whose arguments run on below its first line
/// (rustfmt's `#[derive(` … `)]`, an Angular `@Component({` … `})`) is
/// glued on every line it spans, or the doc above it was lost.
///
/// One forward pass, like [`block_openers`]: walked up from each symbol, a
/// file whose every line starts with an attribute (`#[a] fn f() {}`, legal
/// Rust) took a scan of the file per symbol — a minute and a half for 4 MiB,
/// in the blocking task the file's pane waits on.
///
/// Some languages put more between a doc and its declaration: C#'s
/// attribute lines (`[Fact]`), Erlang's `-spec` (the EDoc goes above it),
/// Haskell's type signature (the Haddock goes above it) — each, with the
/// lines it continues on, glued too.
fn doc_ends(lines: &[&str], lang: Lang) -> Vec<Option<usize>> {
    work::add(lines.len());
    let mut out = Vec::with_capacity(lines.len());
    let mut last: Option<usize> = None;
    // The brackets an attribute above left open, and the lines it may
    // still run on.
    let mut open: Option<(usize, usize)> = None;
    // An Erlang `-spec` or a Haskell signature still running on.
    let mut continued = false;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if let Some(glue) = language_glue(lang, l, &mut continued) {
            if !glue {
                last = Some(i);
            }
            out.push(last);
            continue;
        }
        let glue = match open.take() {
            // A comment's end is not an attribute's: an `@see(` left open in
            // a starless JSDoc block must not swallow the `*/` that closes it.
            Some(_) if t.contains("*/") => false,
            Some((depth, left)) => match run_on(t, depth) {
                Ok(depth) => {
                    open = (left > 1).then(|| (depth, left - 1));
                    true
                }
                Err(rest) => {
                    let rest = rest.trim();
                    let glue = rest.is_empty() || rest.starts_with("//") || glued(rest);
                    open = opens(rest, glue);
                    glue
                }
            },
            None => {
                let glue = glued(t) || (lang == Lang::CSharp && csharp_attribute_line(t));
                open = opens(t, glue);
                glue
            }
        };
        if !glue {
            last = Some(i);
        }
        out.push(last);
    }
    out
}

/// Most lines below its first an attribute's arguments are read as running
/// on: an attribute left unclosed by mistake glues no more than these.
const MAX_ATTRIBUTE_LINES: usize = 64;

/// For `t`, a trimmed line [`glued`] (`glue`) or not: the brackets an
/// attribute on it leaves open, and the lines it may still run on.
fn opens(t: &str, glue: bool) -> Option<(usize, usize)> {
    if !glue || !(t.starts_with("#[") || t.starts_with("#![") || t.starts_with('@')) {
        return None;
    }
    let mut depth = 0;
    let mut rest = t;
    loop {
        match run_on(rest, depth) {
            Ok(0) => return None,
            Ok(left) => return Some((left, MAX_ATTRIBUTE_LINES)),
            Err(after) => (depth, rest) = (0, after),
        }
    }
}

/// `t` read on with `depth` brackets open: `Ok` with how many are still
/// open after it, or `Err` with the text after the one that closes the
/// last. Brackets in a string or character literal are not counted.
fn run_on(t: &str, mut depth: usize) -> Result<usize, &str> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in t.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' | '`' => quote = Some(c),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    return Err(&t[i + 1..]);
                }
            }
            _ => {}
        }
    }
    Ok(depth)
}

/// Whether a line (trimmed) sits between a doc and the signature it
/// documents: attributes or annotations alone — a line declaring an item
/// after them (`#[inline] fn f() {}`, `@Override public String toString()
/// {…}`) is that item's, and so is the doc above it — or a pragma comment.
/// Whether `line` is glue between a doc and its declaration by `lang`'s own
/// rules — `Some(true)` it is, `Some(false)` it ends a run, `None` the
/// general rules decide. `continued` carries a declaration head that runs on
/// over lines: an Erlang `-spec` until its `.`, a Haskell signature over its
/// indented lines.
fn language_glue(lang: Lang, line: &str, continued: &mut bool) -> Option<bool> {
    let t = line.trim();
    match lang {
        Lang::Erlang => {
            if *continued || t.starts_with("-spec") {
                *continued = !t.ends_with('.');
                return Some(true);
            }
            None
        }
        Lang::Haskell => {
            let indented = line.starts_with([' ', '\t']);
            if *continued && indented && !t.is_empty() {
                return Some(true);
            }
            *continued = false;
            // `name :: Type` at the margin: a top-level signature.
            let signature = !indented
                && t.split_once("::").is_some_and(|(name, _)| {
                    let name = name.trim();
                    !name.is_empty()
                        && name.split(',').all(|n| {
                            n.trim()
                                .chars()
                                .all(|c| c.is_alphanumeric() || "_'()".contains(c))
                        })
                });
            if signature {
                *continued = true;
                return Some(true);
            }
            None
        }
        _ => None,
    }
}

/// A line of C# attributes alone: `[Fact]`, `[HttpGet("x"), Authorize]`.
fn csharp_attribute_line(t: &str) -> bool {
    t.starts_with('[')
        && t.ends_with(']')
        && t[1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

fn glued(t: &str) -> bool {
    let pragma = t.starts_with("/*") && !t.starts_with("/**") && t.ends_with("*/");
    pragma || t.starts_with("#!") || crate::outline::only_attributes(t)
}

/// For every line, the nearest line at or above it that contains a
/// block-comment opener (`open`: `/*`), or `None` when there is none.
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
fn block_openers(lines: &[&str], open: &str) -> Vec<Option<usize>> {
    work::add(lines.len());
    let mut out = Vec::with_capacity(lines.len());
    let mut last: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if l.contains(open) {
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

/// Collect a doc block (`/** … */`, `/-- … -/`: see [`BlockSyntax`]) ending
/// on line `end`, stripping the comment markers and, where the syntax has one,
/// the leading ` * ` gutter. Returns `None` for a plain comment (`/* */`: only
/// the doc opener counts as documentation), and for a block that opens
/// `MAX_BLOCK_LINES` or more lines above `end` (i.e. one spanning more than
/// that many lines — the const's own doc states the same bound that way).
fn collect_block(
    lines: &[&str],
    openers: &[Option<usize>],
    end: usize,
    syntax: &BlockSyntax,
) -> Option<String> {
    // `openers` must be the index built for THESE lines. Routing another style
    // here without building it would not look broken — every lookup would miss
    // and the file would simply report no documentation — so say so loudly.
    debug_assert_eq!(openers.len(), lines.len(), "block index not built");
    if !lines[end].contains(syntax.close) {
        return None;
    }
    let start = openers.get(end).copied().flatten()?;
    if end - start >= MAX_BLOCK_LINES {
        return None;
    }
    if !lines[start].contains(syntax.doc) {
        return None;
    }
    work::add(end - start + 1);
    let mut buf: Vec<String> = Vec::new();
    for line in &lines[start..=end] {
        let mut s = line.trim();
        if let Some(p) = s.find(syntax.doc) {
            s = s[p + syntax.doc.len()..].trim_start();
        } else if let Some(p) = s.find(syntax.open) {
            s = s[p + syntax.open.len()..].trim_start();
        }
        if let Some(p) = s.find(syntax.close) {
            s = s[..p].trim_end();
        }
        let s = match syntax.gutter {
            true => s.strip_prefix('*').map(str::trim_start).unwrap_or(s),
            false => s,
        };
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
        Lang::Php => render_inline_tags(s),
        Lang::CSharp => render_xml_doc(s),
        Lang::Erlang => render_edoc(s),
        Lang::Ruby | Lang::Haskell => s.to_string(),
        Lang::Rust
        | Lang::Go
        | Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
        | Lang::Zig
        | Lang::Sql
        | Lang::Lean => s.to_string(),
    }
}

/// C#'s XML doc comments as prose: `<summary>` unwrapped, `<param>` and
/// `<returns>` as lines of their own, `<see cref="X"/>` and `<paramref>` as
/// the names they point at, `<c>` and `<code>` as code — any other tag
/// dropped, its text kept.
fn render_xml_doc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let tag = &rest[open + 1..open + close];
        rest = &rest[open + close + 1..];
        let attr = |name: &str| {
            let at = tag.find(&format!("{name}=\""))? + name.len() + 2;
            let len = tag[at..].find('"')?;
            Some(tag[at..at + len].to_string())
        };
        let word = tag
            .trim_start_matches('/')
            .split([' ', '/'])
            .next()
            .unwrap_or("");
        let closing = tag.starts_with('/');
        match (word, closing) {
            ("see" | "seealso" | "paramref" | "typeparamref", false) => {
                let target = attr("cref")
                    .or_else(|| attr("name"))
                    .or_else(|| attr("langword"));
                if let Some(target) = target {
                    // `T:Namespace.Type` → `Type`.
                    let target = target
                        .rsplit([':', '.'])
                        .next()
                        .unwrap_or(&target)
                        .to_string();
                    out.push('`');
                    out.push_str(&target);
                    out.push('`');
                }
            }
            ("param" | "typeparam", false) => {
                out.push_str("\n- `");
                out.push_str(&attr("name").unwrap_or_default());
                out.push_str("`: ");
            }
            ("returns", false) => out.push_str("\nReturns "),
            ("exception", false) => {
                out.push_str("\nThrows `");
                out.push_str(&attr("cref").unwrap_or_default());
                out.push_str("`: ");
            }
            ("para" | "br", _) => out.push_str("\n\n"),
            ("c" | "code", _) => out.push('`'),
            _ => {}
        }
    }
    out.push_str(rest);
    out.lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Erlang's EDoc tags as prose: `@doc` unwrapped, `@param`/`@returns`/
/// `@see` as lines of their own, `` `x' `` quoting as code.
fn render_edoc(s: &str) -> String {
    let mut lines = Vec::new();
    for line in s.lines() {
        let t = line.trim();
        let t = t.strip_prefix("@doc").map(str::trim_start).unwrap_or(t);
        let t = match t.split_once(' ') {
            Some(("@param", rest)) => format!("- {rest}"),
            Some(("@returns" | "@return", rest)) => format!("Returns {rest}"),
            Some(("@see", rest)) => format!("See {rest}"),
            _ => t.to_string(),
        };
        lines.push(edoc_quotes(&t));
    }
    lines.join("\n").trim().to_string()
}

/// EDoc's `` `code' `` quoting as Markdown's `` `code` ``: the `'` closing
/// a backquote, and no other apostrophe.
fn edoc_quotes(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut open = false;
    for c in line.chars() {
        match c {
            '`' => {
                open = true;
                out.push('`');
            }
            '\'' if open => {
                open = false;
                out.push('`');
            }
            c => out.push(c),
        }
    }
    out
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
    let role = &after_first[..close];
    let role = role.rsplit(':').next().unwrap_or(role);
    // `:ref:`the guide <guide>``: the words, not the label.
    let (target, titled) = match target.rsplit_once('<').filter(|(_, l)| l.ends_with('>')) {
        Some((title, _)) if !title.trim().is_empty() => (title.trim(), true),
        _ => (target, false),
    };
    let shown = if titled {
        target
    } else if let Some(t) = target.strip_prefix('~') {
        t.rsplit(['.', ':']).next().unwrap_or(t)
    } else {
        target.strip_prefix('.').unwrap_or(target)
    };
    let shown = match role {
        _ if titled => shown.to_string(),
        "pep" => format!("PEP {shown}"),
        "rfc" => format!("RFC {shown}"),
        "issue" | "pr" | "gh" => format!("#{shown}"),
        "ref" | "doc" | "term" | "abbr" | "dfn" | "title" | "emphasis" | "strong" => {
            shown.to_string()
        }
        // `func`, `class`, `meth`, `attr`, `data`, `exc`, `obj`, `mod`,
        // `const`, `kbd`, `file`, `envvar`, …: names and things typed.
        _ if !shown.contains('`') => format!("`{shown}`"),
        _ => shown.to_string(),
    };
    // leading ':' (1) + name + ":`" (2) + target + closing '`' (1)
    Some((shown, 1 + close + 2 + target_end + 1))
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
    // A raw or unicode literal is a docstring too: `r"""…"""` is the usual
    // form of one that holds a backslash.
    let first = first
        .strip_prefix(|c| matches!(c, 'r' | 'R' | 'u' | 'U'))
        .filter(|rest| rest.starts_with(['"', '\'']))
        .unwrap_or(first);

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
                    return non_empty(&python_doc_markdown(&dedent(&buf)));
                }
                buf.push(lines[j].to_string());
                j += 1;
            }
            return non_empty(&python_doc_markdown(&dedent(&buf)));
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

/// Google style's section headings, as their line reads less its colon.
const GOOGLE_SECTIONS: &[&str] = &[
    "Args",
    "Arguments",
    "Parameters",
    "Params",
    "Keyword Args",
    "Keyword Arguments",
    "Kwargs",
    "Other Parameters",
    "Attributes",
    "Raises",
    "Exceptions",
    "Warns",
    "Returns",
    "Return",
    "Yields",
    "Yield",
    "Example",
    "Examples",
    "Note",
    "Notes",
    "Warning",
    "Warnings",
    "See Also",
    "Todo",
    "References",
];

/// NumPy style's section headings, each over a rule of dashes.
const NUMPY_SECTIONS: &[&str] = &[
    "Parameters",
    "Other Parameters",
    "Returns",
    "Yields",
    "Receives",
    "Raises",
    "Warns",
    "Attributes",
    "Methods",
    "See Also",
    "Notes",
    "References",
    "Examples",
    "Warnings",
];

/// A Python docstring's sections as Markdown, as the Docs view and the
/// cards render it: Google style's (`Args:` over indented `name (type):
/// text` entries), NumPy style's (`Parameters` over a rule of dashes,
/// `name : type` lines with their text indented under them) and Sphinx
/// fields (`:param name: text`, `:returns:`, `:raises E:`) — each section a
/// bold heading, each entry a bullet with its name in code, an example of
/// `>>>` lines a block of Python. As written, an indented entry rendered as
/// a code block and Sphinx's fields as one run-on paragraph. Prose is left
/// as it is.
pub fn python_doc_markdown(doc: &str) -> String {
    let rest = rest_blocks(&doc.lines().collect::<Vec<_>>());
    let lines: Vec<&str> = rest.iter().map(String::as_str).collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let head = line.trim();
        // Google: `Args:` alone on a line of its own, its body indented.
        if indent_of(line) == 0
            && let Some(name) = head
                .strip_suffix(':')
                .filter(|n| GOOGLE_SECTIONS.contains(n))
        {
            let mut end = i + 1;
            while end < lines.len() && (lines[end].trim().is_empty() || indent_of(lines[end]) > 0) {
                end += 1;
            }
            while end > i + 1 && lines[end - 1].trim().is_empty() {
                end -= 1;
            }
            if end > i + 1 {
                push_block(
                    &mut out,
                    doc_section(name, &dedent_lines(&lines[i + 1..end]), false),
                );
                i = end;
                continue;
            }
        }
        // NumPy: a heading over a rule of dashes, its body to the next one.
        if indent_of(line) == 0
            && NUMPY_SECTIONS.contains(&head)
            && lines.get(i + 1).is_some_and(|l| is_rule(l))
        {
            let mut end = i + 2;
            while end < lines.len() && !numpy_heading_at(&lines, end) {
                end += 1;
            }
            let next = end;
            while end > i + 2 && lines[end - 1].trim().is_empty() {
                end -= 1;
            }
            push_block(
                &mut out,
                doc_section(head, &dedent_lines(&lines[i + 2..end]), true),
            );
            i = next;
            continue;
        }
        // Sphinx: a run of `:field:` lines, each with its indented
        // continuation.
        if sphinx_field(head).is_some() {
            let mut end = i;
            let mut fields: Vec<(String, String)> = Vec::new();
            while end < lines.len() {
                let l = lines[end];
                if let Some(field) = sphinx_field(l.trim()) {
                    fields.push(field);
                } else if indent_of(l) > 0 && !l.trim().is_empty() && !fields.is_empty() {
                    let last = fields.last_mut().expect("a field before");
                    last.1.push(' ');
                    last.1.push_str(l.trim());
                } else {
                    break;
                }
                end += 1;
            }
            push_block(&mut out, sphinx_markdown(&fields));
            i = end;
            continue;
        }
        out.push(line.to_string());
        i += 1;
    }
    let mut text = out.join("\n");
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    text.trim().to_string()
}

/// Most lines a reST directive's body may span: a docstring's note is a
/// paragraph or two, an example a screenful.
const MAX_DIRECTIVE_LINES: usize = 400;

/// reStructuredText's block markup as Markdown, ahead of the sections:
/// an admonition (`.. note::`, `.. warning::`, `.. seealso::`, …) as a
/// quote headed with its kind, `.. versionadded::`/`versionchanged::`/
/// `deprecated::` as an italic line with what it says, `.. code-block::`
/// (and `code`, `sourcecode`, `doctest`) and a paragraph ending `::` as a
/// fenced block of code, a hyperlink as a Markdown link. A comment or a
/// target (`.. _name:`) is dropped; any other directive keeps its text.
/// As written, these showed as `.. note::` lines and indented runs the
/// Markdown read as unhighlighted code.
fn rest_blocks(lines: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    // A directive's or literal block's body: the lines after `at` indented
    // past `indent`, blank ones among them; returned dedented, and the
    // index after it.
    let body = |at: usize, indent: usize| -> (Vec<String>, usize) {
        let mut end = at + 1;
        while end < lines.len()
            && end - at <= MAX_DIRECTIVE_LINES
            && (lines[end].trim().is_empty() || indent_of(lines[end]) > indent)
        {
            end += 1;
        }
        let mut last = end;
        while last > at + 1 && lines[last - 1].trim().is_empty() {
            last -= 1;
        }
        (dedent_lines(&lines[at + 1..last]), end)
    };
    let fence = |out: &mut Vec<String>, language: &str, code: &[String]| {
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push(format!("```{language}"));
        let skip = code.iter().take_while(|l| l.trim().is_empty()).count();
        out.extend(code[skip..].iter().cloned());
        out.push("```".to_string());
        out.push(String::new());
    };
    while i < lines.len() {
        let line = lines[i];
        let indent = indent_of(line);
        let head = line.trim();
        // Only markup at the left edge: an indented one is inside a
        // section's body, which reads its own (`prose_and_code`).
        if let Some(directive) = head.strip_prefix(".. ").filter(|_| indent == 0) {
            let (mut text, end) = body(i, indent);
            i = end;
            let Some((name, args)) = directive.split_once("::") else {
                // `.. _target:` and `.. a comment`: nothing to read.
                continue;
            };
            let (name, args) = (name.trim(), args.trim());
            // A directive's options (`:linenos:`) open its body.
            let options = text
                .iter()
                .take_while(|l| l.trim_start().starts_with(':') && !l.trim().is_empty())
                .count();
            text.drain(..options);
            let blank = text.iter().take_while(|l| l.trim().is_empty()).count();
            text.drain(..blank);
            let prose = || {
                text.iter()
                    .map(|l| l.trim())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let label = match name {
                "note" => Some("Note"),
                "warning" => Some("Warning"),
                "tip" => Some("Tip"),
                "hint" => Some("Hint"),
                "important" => Some("Important"),
                "caution" => Some("Caution"),
                "danger" => Some("Danger"),
                "attention" => Some("Attention"),
                "error" => Some("Error"),
                "seealso" => Some("See also"),
                "admonition" => Some(args),
                _ => None,
            };
            if out.last().is_some_and(|l| !l.trim().is_empty()) {
                out.push(String::new());
            }
            match (name, label) {
                (_, Some(label)) => {
                    let lead = match name {
                        "admonition" => String::new(),
                        _ => args.to_string(),
                    };
                    let mut said: Vec<String> = Vec::new();
                    if !lead.is_empty() {
                        said.push(lead);
                    }
                    said.extend(text.iter().map(|l| l.trim().to_string()));
                    let mut first = true;
                    for line in said {
                        out.push(match (first, line.is_empty()) {
                            (true, _) => format!("> **{label}:** {line}").trim_end().to_string(),
                            (false, true) => ">".to_string(),
                            (false, false) => format!("> {line}"),
                        });
                        first = false;
                    }
                    if first {
                        out.push(format!("> **{label}**"));
                    }
                    out.push(String::new());
                }
                ("versionadded" | "versionchanged" | "deprecated", _) => {
                    let (version, inline) =
                        args.split_once(char::is_whitespace).unwrap_or((args, ""));
                    let what = match name {
                        "versionadded" => "New in version",
                        "versionchanged" => "Changed in version",
                        _ => "Deprecated since version",
                    };
                    let said = [inline.trim().to_string(), prose()]
                        .into_iter()
                        .filter(|p| !p.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ");
                    out.push(match said.is_empty() {
                        true => format!("*{what} {version}.*"),
                        false => format!("*{what} {version}:* {said}"),
                    });
                    out.push(String::new());
                }
                ("code-block" | "code" | "sourcecode" | "doctest" | "testcode", _) => {
                    let language = match (name, args) {
                        ("doctest" | "testcode", _) | (_, "") | (_, "pycon") => "python",
                        (_, language) => language,
                    };
                    fence(&mut out, language, &text);
                }
                _ => out.extend(text),
            }
            continue;
        }
        // `Example::` or a lone `::`: a literal block, Python's code.
        if indent == 0 && head.ends_with("::") && lines.get(i + 1).is_some() {
            let (code, end) = body(i, indent);
            if !code.iter().all(|l| l.trim().is_empty()) {
                let intro = head.trim_end_matches(':').trim_end();
                if !intro.is_empty() {
                    out.push(format!("{}{intro}:", " ".repeat(indent)));
                }
                fence(&mut out, "python", &code);
                i = end;
                continue;
            }
        }
        out.push(rest_links(line));
        i += 1;
    }
    out
}

/// reST hyperlinks as Markdown's: `` `text <url>`_ `` as `[text](url)`,
/// a bare reference (`` `text`_ ``) as its text.
fn rest_links(line: &str) -> String {
    if !line.contains("`_") {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else {
            break;
        };
        let inner = &after[..close];
        let tail = &after[close + 1..];
        if tail.starts_with('_') {
            let skip = if tail.starts_with("__") { 2 } else { 1 };
            out.push_str(&rest[..open]);
            match inner.rsplit_once('<').filter(|(_, u)| u.ends_with('>')) {
                Some((text, url)) => {
                    out.push_str(&format!("[{}]({})", text.trim(), url.trim_end_matches('>')));
                }
                None => out.push_str(inner),
            }
            rest = &tail[skip..];
        } else {
            out.push_str(&rest[..open + 1 + close + 1]);
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// A line's leading whitespace, in chars.
fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

/// A rule of dashes under a NumPy heading.
fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3 && t.chars().all(|c| c == '-')
}

/// Whether a NumPy section's heading starts at `lines[at]`.
fn numpy_heading_at(lines: &[&str], at: usize) -> bool {
    indent_of(lines[at]) == 0
        && NUMPY_SECTIONS.contains(&lines[at].trim())
        && lines.get(at + 1).is_some_and(|l| is_rule(l))
}

/// `lines` less the indentation they all share.
fn dedent_lines(lines: &[&str]) -> Vec<String> {
    let common = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent_of(l))
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            l.chars()
                .skip(common)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// `block` set apart by blank lines from what is around it.
fn push_block(out: &mut Vec<String>, block: Vec<String>) {
    if out.last().is_some_and(|l| !l.trim().is_empty()) {
        out.push(String::new());
    }
    out.extend(block);
    out.push(String::new());
}

/// The one heading a section is shown under, whichever style wrote it:
/// Google's `Args`, NumPy's and Sphinx's `Parameters` were two names for one
/// thing side by side in the Docs view.
fn section_heading(name: &str) -> &str {
    match name {
        "Args" | "Arguments" | "Parameters" | "Params" => "Parameters",
        "Keyword Args" | "Keyword Arguments" | "Kwargs" => "Keyword Arguments",
        "Return" | "Returns" => "Returns",
        "Yield" | "Yields" => "Yields",
        "Raises" | "Exceptions" => "Raises",
        "Example" | "Examples" => "Examples",
        "Note" | "Notes" => "Notes",
        "Warning" | "Warnings" => "Warnings",
        other => other,
    }
}

/// Whether `s` reads as a type (`bool`, `dict[str, int]`, `Optional[Cart]`)
/// rather than words: no space outside its brackets.
fn reads_as_type(s: &str) -> bool {
    let mut depth = 0i32;
    s.chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && s.len() <= 80
        && s.chars().all(|c| {
            match c {
                '[' | '(' => depth += 1,
                ']' | ')' => depth -= 1,
                _ => {}
            }
            !(c.is_whitespace() && depth == 0)
        })
}

/// A section's prose with its code kept as code: a run of lines indented
/// under the prose, or of `>>>` lines and what they print, is a block of
/// Python — trimmed into the prose, `for x in xs:` and its body ran
/// together as one sentence. A line ending in reStructuredText's `::`,
/// which introduces such a block, ends in one colon.
fn prose_and_code(body: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let line = &body[i];
        let doctest = line.trim_start().starts_with(">>>");
        if doctest || (indent_of(line) > 0 && !line.trim().is_empty()) {
            let mut end = i + 1;
            while end < body.len() {
                let next = &body[end];
                let more = if doctest {
                    !next.trim().is_empty()
                } else {
                    next.trim().is_empty() || indent_of(next) > 0
                };
                if !more {
                    break;
                }
                end += 1;
            }
            let mut run: Vec<&str> = body[i..end].iter().map(String::as_str).collect();
            while run.last().is_some_and(|l| l.trim().is_empty()) {
                run.pop();
            }
            if out.last().is_some_and(|l: &String| !l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push("```python".to_string());
            out.extend(dedent_lines(&run));
            out.push("```".to_string());
            out.push(String::new());
            i = end;
            continue;
        }
        let text = line.trim();
        out.push(match text.strip_suffix("::") {
            Some(intro) => format!("{intro}:"),
            None => text.to_string(),
        });
        i += 1;
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out
}

/// A section of `name` with its dedented `body`: a list of entries where
/// the section lists them (NumPy's returns and yields too, a type over its
/// text), Python where it gives examples or code, a return's type in code
/// as NumPy's and Sphinx's are, else its paragraphs — never indented, which
/// Markdown would show as code. Headed as every style's is
/// ([`section_heading`]).
fn doc_section(name: &str, body: &[String], numpy: bool) -> Vec<String> {
    let mut out = vec![format!("**{}**", section_heading(name)), String::new()];
    let lists = matches!(
        name,
        "Args"
            | "Arguments"
            | "Parameters"
            | "Params"
            | "Keyword Args"
            | "Keyword Arguments"
            | "Kwargs"
            | "Other Parameters"
            | "Attributes"
            | "Raises"
            | "Exceptions"
            | "Warns"
            | "Receives"
            | "Methods"
    ) || (numpy && matches!(name, "Returns" | "Yields"));
    if lists {
        // An entry starts unindented; what is indented under it is its text.
        let mut entries: Vec<(String, String)> = Vec::new();
        for line in body {
            if line.trim().is_empty() {
                continue;
            }
            if indent_of(line) == 0 || entries.is_empty() {
                entries.push((line.trim().to_string(), String::new()));
            } else if let Some(last) = entries.last_mut() {
                if !last.1.is_empty() {
                    last.1.push(' ');
                }
                last.1.push_str(line.trim());
            }
        }
        for (head, more) in entries {
            out.push(doc_entry(&head, &more, numpy));
        }
    } else if name.starts_with("Example") && !body.iter().any(|l| l.trim_end().ends_with("::")) {
        // An example is code, whether doctest or not: `config = load()`
        // over `for x in config:` read as one sentence. One that says what
        // it shows first (`Run it like so::`) is prose and code.
        let mut code: Vec<String> = body.to_vec();
        while code.last().is_some_and(|l| l.trim().is_empty()) {
            code.pop();
        }
        out.push("```python".to_string());
        out.extend(code);
        out.push("```".to_string());
    } else if matches!(section_heading(name), "Returns" | "Yields")
        && let Some(first) = body.iter().position(|l| !l.trim().is_empty())
        && let Some((kind, text)) = body[first].trim().split_once(':')
        && reads_as_type(kind.trim())
    {
        // `bool: whether it moved` — the type in code, as NumPy's and
        // Sphinx's returns are.
        out.push(
            format!("`{}`: {}", kind.trim(), text.trim())
                .trim_end()
                .to_string(),
        );
        out.extend(body[first + 1..].iter().map(|l| l.trim().to_string()));
    } else {
        out.extend(prose_and_code(body));
    }
    out
}

/// One entry of a section as a bullet: `- `name` (type): text`. Google
/// writes `name (type): text`; NumPy `name : type` over its text.
fn doc_entry(head: &str, more: &str, numpy: bool) -> String {
    let join = |a: &str, b: &str| match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a} {b}"),
    };
    let (name, kind, text) = if numpy {
        match head.split_once(" : ").or_else(|| head.split_once(": ")) {
            Some((name, kind)) => (name.trim(), kind.trim(), more.to_string()),
            None => (head.trim(), "", more.to_string()),
        }
    } else {
        match head.split_once(':') {
            Some((name, text)) => {
                let (name, kind) = match name.split_once('(') {
                    Some((n, k)) => (n.trim(), k.trim_end().trim_end_matches(')').trim()),
                    None => (name.trim(), ""),
                };
                (name, kind, join(text.trim(), more))
            }
            None => (head.trim(), "", more.to_string()),
        }
    };
    let mut out = format!("- `{name}`");
    if !kind.is_empty() {
        out.push_str(&format!(" ({kind})"));
    }
    if !text.is_empty() {
        out.push_str(": ");
        out.push_str(&text);
    }
    out
}

/// A Sphinx field (`:param int x: text`) as its role and words: the role
/// with its argument (`param int x`), and the text after the closing colon.
fn sphinx_field(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix(':')?;
    let (role, text) = rest.split_once(':')?;
    let word = role.split_whitespace().next()?;
    matches!(
        word,
        "param"
            | "parameter"
            | "arg"
            | "argument"
            | "key"
            | "keyword"
            | "type"
            | "returns"
            | "return"
            | "rtype"
            | "raises"
            | "raise"
            | "except"
            | "exception"
            | "var"
            | "ivar"
            | "cvar"
            | "vartype"
            | "yields"
            | "yield"
    )
    .then(|| (role.trim().to_string(), text.trim().to_string()))
}

/// Sphinx fields as sections: the parameters (with the types `:type:`
/// gives them), what is returned or yielded, what is raised, the
/// attributes.
fn sphinx_markdown(fields: &[(String, String)]) -> Vec<String> {
    let mut types: HashMap<String, String> = HashMap::new();
    let mut rtype = String::new();
    for (role, text) in fields {
        let mut words = role.split_whitespace();
        match (words.next(), words.next_back()) {
            (Some("type" | "vartype"), Some(name)) => {
                types.insert(name.to_string(), text.clone());
            }
            (Some("rtype"), _) => rtype = text.clone(),
            _ => {}
        }
    }
    let mut params: Vec<String> = Vec::new();
    let mut attrs: Vec<String> = Vec::new();
    let mut raises: Vec<String> = Vec::new();
    let mut returns: Vec<String> = Vec::new();
    for (role, text) in fields {
        let words: Vec<&str> = role.split_whitespace().collect();
        let entry = |name: &str, kind: &str| {
            let kind = if kind.is_empty() {
                types.get(name).map(String::as_str).unwrap_or("")
            } else {
                kind
            };
            doc_entry(
                &if kind.is_empty() {
                    format!("{name}: {text}")
                } else {
                    format!("{name} ({kind}): {text}")
                },
                "",
                false,
            )
        };
        let name = words.last().copied().unwrap_or("");
        let kind = if words.len() > 2 {
            words[1..words.len() - 1].join(" ")
        } else {
            String::new()
        };
        match words.first().copied() {
            Some("param" | "parameter" | "arg" | "argument" | "key" | "keyword")
                if words.len() > 1 =>
            {
                params.push(entry(name, &kind));
            }
            Some("var" | "ivar" | "cvar") if words.len() > 1 => attrs.push(entry(name, &kind)),
            Some("raises" | "raise" | "except" | "exception") => raises.push(doc_entry(
                &format!(
                    "{}: {text}",
                    words.get(1..).map(|w| w.join(" ")).unwrap_or_default()
                ),
                "",
                false,
            )),
            Some("returns" | "return" | "yields" | "yield") => returns.push(text.clone()),
            _ => {}
        }
    }
    let mut out = Vec::new();
    let mut section = |name: &str, items: Vec<String>| {
        if items.is_empty() {
            return;
        }
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(format!("**{name}**"));
        out.push(String::new());
        out.extend(items);
    };
    section("Parameters", params);
    if !returns.is_empty() || !rtype.is_empty() {
        let text = returns.join(" ");
        let line = match (rtype.is_empty(), text.is_empty()) {
            (true, _) => text,
            (false, true) => format!("`{rtype}`"),
            (false, false) => format!("`{rtype}`: {text}"),
        };
        section("Returns", vec![line]);
    }
    section("Raises", raises);
    section("Attributes", attrs);
    out
}

/// Strip the common leading indentation from a docstring's lines.
///
/// The first line is the text right after the opening quotes, so it carries no
/// indentation of its own: it is left out of the measure and only trimmed, as
/// `inspect.cleandoc` does. Counted in, the usual `"""Summary.` layout made
/// the common indent 0, and every later line kept its four spaces, which
/// markdown renders as a code block.
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
    let Some((first, rest)) = lines.split_first() else {
        return String::new();
    };
    let min_indent = rest
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
        .min()
        .unwrap_or(0);
    std::iter::once(first.trim_start())
        .chain(rest.iter().map(|l| match l.char_indices().nth(min_indent) {
            Some((b, _)) => &l[b..],
            // Shorter than the common indent, so it is blank by construction:
            // every non-blank line has at least `min_indent` chars.
            None => "",
        }))
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
    fn a_doc_above_an_attribute_wrapped_over_lines_is_found() {
        let rust = "/// A config.\n#[derive(\n    Debug, Clone, // all of them\n    PartialEq,\n)]\n#[serde(rename_all = \"camelCase\")]\npub struct Config;\n";
        let docs = docs_of(rust, "rust");
        assert_eq!(docs.get(&7).map(String::as_str), Some("A config."));
        let items = crate::outline::extract_located(rust, "rust");
        assert_eq!(
            extract_located(rust, "rust", &items)
                .get(&7)
                .map(String::as_str),
            Some("A config.")
        );
        // Two attributes on the line that closes the first, the second left
        // open in turn; a `)` in a string closes nothing.
        let rust = "/// A config.\n#[derive(\n    Debug,\n)] #[error(\n    \"missing )\"\n)]\npub struct Config;\n";
        assert_eq!(
            docs_of(rust, "rust").get(&7).map(String::as_str),
            Some("A config.")
        );
        let ts = "/** The root. */\n@Component({\n  selector: 'app-root',\n  template: '<p>(</p>',\n})\nexport class AppComponent {}\n";
        assert_eq!(
            docs_of(ts, "typescript").get(&6).map(String::as_str),
            Some("The root.")
        );
    }

    #[test]
    fn an_attribute_left_open_glues_no_further_than_its_bound() {
        // Closed on the same line, nothing runs on: the plain comment in
        // between still parts `b` from `a`'s doc.
        let src = "/// Of a.\n#[derive(Debug)]\npub struct A;\n// not a doc\npub fn b() {}\n";
        let docs = docs_of(src, "rust");
        assert_eq!(docs.get(&3).map(String::as_str), Some("Of a."));
        assert!(!docs.contains_key(&5), "{docs:?}");
        // Never closed: the lines below it glue only up to the bound, so a
        // function far below borrows nothing. (A parser reads the rest of the
        // file into the attribute, so the function is named by hand.)
        let mut src = String::from("/// Of nothing.\n#[cfg(\n");
        src.push_str(&"\n".repeat(MAX_ATTRIBUTE_LINES + 1));
        src.push_str("pub fn far() {}\n");
        let line = src.lines().count();
        let far = |line| Symbol {
            name: "far".into(),
            kind: "function".into(),
            line,
            end_line: line,
        };
        let docs = extract(&src, "rust", &[far(line)]);
        assert!(docs.is_empty(), "{docs:?}");
        // Within it, it is the attribute's.
        let src = "/// Of far.\n#[cfg(\n\npub fn far() {}\n";
        assert_eq!(
            extract(src, "rust", &[far(4)]).get(&4).map(String::as_str),
            Some("Of far.")
        );
        // An `@see(` left open in a starless JSDoc ends with the comment.
        let js = "/**\n  Adds.\n  @see(\n*/\nfunction add() {}\n";
        assert_eq!(
            docs_of(js, "javascript")
                .get(&5)
                .map(|d| d.contains("Adds")),
            Some(true)
        );
    }

    #[test]
    fn the_peek_reads_the_doc_above_where_the_declaration_starts() {
        let (symbols, docs) = outline_and_peek(
            "/** Larger. */\ntemplate <typename T>\nT larger(T a, T b) { return a; }\n",
            "cpp",
        );
        assert!(
            symbols.iter().any(|s| s.name == "larger" && s.line == 3),
            "{symbols:?}"
        );
        assert_eq!(docs.get(&3).map(String::as_str), Some("Larger."));
        let (_, docs) = outline_and_peek(
            "/** Adds. */\nstatic int\nadd(int a, int b) { return a + b; }\n",
            "c",
        );
        assert_eq!(docs.get(&3).map(String::as_str), Some("Adds."));
        let (_, docs) = outline_and_peek("/** Runs. */\nexport\nfunction run() {}\n", "javascript");
        assert_eq!(docs.get(&3).map(String::as_str), Some("Runs."));
        // Kept to the peek's length.
        let long = format!("/** {} */\nfunction f() {{}}\n", "word ".repeat(MAX_DOC));
        let (_, docs) = outline_and_peek(&long, "javascript");
        assert_eq!(docs.get(&2).map(|d| d.chars().count()), Some(MAX_DOC + 1));
    }

    /// A module's own doc (`//!`) is not the item below it's, and `////`
    /// is no doc at all.
    #[test]
    fn inner_docs_and_four_slashes_are_not_an_items_doc() {
        assert!(docs_of("//! The module.\npub fn first() {}\n", "rust").is_empty());
        assert!(docs_of("//// A note.\npub fn f() {}\n", "rust").is_empty());
        let docs = docs_of("//! The module.\n/// The fn.\npub fn f() {}\n", "rust");
        assert_eq!(docs.get(&3).map(String::as_str), Some("The fn."));
        let docs = docs_of("//// A note.\n/// The fn.\npub fn f() {}\n", "rust");
        assert_eq!(docs.get(&3).map(String::as_str), Some("The fn."));
    }

    #[test]
    fn rust_plain_comment_is_not_a_doc() {
        let src = "// internal note\npub fn add() {}\n";
        assert!(docs_of(src, "rust").is_empty());
    }

    /// A raw or unicode docstring is a docstring; a string named by
    /// something else is not one.
    #[test]
    fn a_raw_docstring_is_a_docstring() {
        let src = "def f():\n    r\"\"\"Match \\d+.\"\"\"\n\ndef g():\n    U'Hi.'\n\ndef h():\n    rx = 'no'\n";
        let docs = docs_of(src, "python");
        assert_eq!(docs.get(&1).map(String::as_str), Some("Match \\d+."));
        assert_eq!(docs.get(&4).map(String::as_str), Some("Hi."));
        assert!(!docs.contains_key(&7), "{docs:?}");
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
        // reST roles as their target, honoring ~ and leading dot: a name in
        // code, a cross-reference's words as words.
        assert_eq!(
            clean_doc_text("A :class:`Request` object", Lang::Python),
            "A `Request` object"
        );
        assert_eq!(
            clean_doc_text("uses :py:class:`Foo`", Lang::Python),
            "uses `Foo`"
        );
        assert_eq!(
            clean_doc_text("call :meth:`~scrapy.Request.replace`", Lang::Python),
            "call `replace`"
        );
        assert_eq!(
            clean_doc_text("see :ref:`the guide <guide>` and :pep:`8`", Lang::Python),
            "see the guide and PEP 8"
        );
        assert_eq!(
            clean_doc_text(":doc:`/tutorial`, :issue:`42`", Lang::Python),
            "/tutorial, #42"
        );
        // Plain colons (not a role) are untouched.
        assert_eq!(
            clean_doc_text("note: this is fine", Lang::Python),
            "note: this is fine"
        );
    }

    /// reST's block markup as Markdown: an admonition as a quote headed with
    /// its kind, a version note as an italic line, a code block and a `::`
    /// paragraph's block as fenced code, a hyperlink as a link; a comment and
    /// a target dropped.
    #[test]
    fn python_rest_directives_render_as_markdown() {
        let doc = "\
Register a URL rule.

.. _rules:

.. note::
   Endpoints default to the
   view's name.

   Two paragraphs.

.. versionchanged:: 2.0
    The ``provide_automatic_options`` default moved.

.. versionadded:: 0.7

.. code-block:: python
   :linenos:

   app.add_url_rule(\"/\", view_func=index)

Use it like this::

    @app.route(\"/\")
    def index():
        return \"Hi\"

.. this is a comment
   that spans two lines

See `the docs <https://flask.palletsprojects.com/>`_ and `Rules`_.
";
        let md = python_doc_markdown(doc);
        let want = "\
Register a URL rule.

> **Note:** Endpoints default to the
> view's name.
>
> Two paragraphs.

*Changed in version 2.0:* The ``provide_automatic_options`` default moved.

*New in version 0.7.*

```python
app.add_url_rule(\"/\", view_func=index)
```

Use it like this:

```python
@app.route(\"/\")
def index():
    return \"Hi\"
```

See [the docs](https://flask.palletsprojects.com/) and Rules.";
        assert_eq!(md, want);
        // A seealso with its text on the directive's own line, and a titled
        // admonition.
        assert_eq!(
            python_doc_markdown(".. seealso:: :func:`url_for`\n"),
            "> **See also:** :func:`url_for`"
        );
        assert_eq!(
            python_doc_markdown(".. admonition:: Thread safety\n\n   Not safe.\n"),
            "> **Thread safety:** Not safe."
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
    fn google_docstring_sections_read_as_markdown() {
        let doc = "Move the cart.\n\nArgs:\n    cart (Cart): the cart to move.\n    to: where it goes,\n        a place name.\n\nReturns:\n    bool: whether it moved,\n    once.\n\nRaises:\n    ValueError: if `to` is empty.\n\nExample:\n    >>> move(c, \"home\")\n    True\n\nNote:\n    Slow on\n    large carts.";
        assert_eq!(
            python_doc_markdown(doc),
            "Move the cart.\n\n**Parameters**\n\n- `cart` (Cart): the cart to move.\n- `to`: where it goes, a place name.\n\n\
             **Returns**\n\n`bool`: whether it moved,\nonce.\n\n**Raises**\n\n- `ValueError`: if `to` is empty.\n\n\
             **Examples**\n\n```python\n>>> move(c, \"home\")\nTrue\n```\n\n**Notes**\n\nSlow on\nlarge carts."
        );
        // Code in an example, doctest or not, stays code; prose that
        // introduces it (`::`) stays prose.
        assert_eq!(
            python_doc_markdown(
                "Load.\n\nExample:\n    config = load()\n    for x in config:\n        print(x)"
            ),
            "Load.\n\n**Examples**\n\n```python\nconfig = load()\nfor x in config:\n    print(x)\n```"
        );
        assert_eq!(
            python_doc_markdown(
                "Load.\n\nNote:\n    Run it like so::\n\n        load(fast=True)\n\n    It is slow otherwise."
            ),
            "Load.\n\n**Notes**\n\nRun it like so:\n\n```python\nload(fast=True)\n```\n\nIt is slow otherwise."
        );
        // A return that names no type is prose.
        assert_eq!(
            python_doc_markdown("Get.\n\nReturns:\n    The value: in cents."),
            "Get.\n\n**Returns**\n\nThe value: in cents."
        );
    }

    #[test]
    fn numpy_docstring_sections_read_as_markdown() {
        let doc = "Scale a vector.\n\nParameters\n----------\nv : array_like\n    The vector.\nk : float, optional\n    The factor,\n    1 by default.\n\nReturns\n-------\nndarray\n    The scaled vector.";
        assert_eq!(
            python_doc_markdown(doc),
            "Scale a vector.\n\n**Parameters**\n\n- `v` (array_like): The vector.\n- `k` (float, optional): The factor, 1 by default.\n\n\
             **Returns**\n\n- `ndarray`: The scaled vector."
        );
    }

    #[test]
    fn sphinx_fields_read_as_markdown() {
        let doc = "Open a file.\n\n:param path: where it is.\n:type path: str\n:param int mode: how,\n    as os.open takes it.\n:returns: the handle.\n:rtype: File\n:raises OSError: if it cannot.";
        assert_eq!(
            python_doc_markdown(doc),
            "Open a file.\n\n**Parameters**\n\n- `path` (str): where it is.\n- `mode` (int): how, as os.open takes it.\n\n\
             **Returns**\n\n`File`: the handle.\n\n**Raises**\n\n- `OSError`: if it cannot."
        );
    }

    /// Prose, a word ending in a colon that names no section, and a heading
    /// with nothing under it are left as they are.
    #[test]
    fn plain_docstrings_are_left_as_they_are() {
        for doc in [
            "Just a summary.",
            "Summary.\n\nMore words: here.\n\n- a list\n- of things",
            "Summary.\n\nNote:",
            "Summary.\n\nCaveats:\n    not a section.",
        ] {
            assert_eq!(python_doc_markdown(doc), doc, "{doc:?}");
        }
    }

    #[test]
    fn python_docstring_with_a_summary_on_the_opening_line_is_dedented() {
        // PEP 257's layout: the summary follows the opening quotes, so that line
        // has no indent. Counted in, it made the common indent 0, and the body
        // kept four spaces — a markdown code block in the Docs view.
        let src = "def f(x):\n    \"\"\"Return x doubled.\n\n    Args:\n        x: the value.\n    \"\"\"\n    return 2 * x\n";
        let docs = docs_of(src, "python");
        let d = docs.values().next().expect("a docstring");
        assert_eq!(
            d,
            "Return x doubled.\n\n**Parameters**\n\n- `x`: the value."
        );
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

    /// A line declaring an item after its attributes or annotations is that
    /// item's, and so is the doc above it: not the next item's. And the
    /// lines glued between a doc and its signature are passed over in one
    /// pass for the file — walked up from each symbol, a file of
    /// `#[a] fn f() {}` lines took a scan of the file per symbol, 98 s for
    /// 4 MiB. Asserted on the lines visited.
    #[test]
    fn an_attribute_on_its_items_line_is_not_glued_to_the_next_one() {
        const N: usize = 20_000;
        let mut src = String::from("/// The first one's.\n");
        let mut symbols = Vec::new();
        for i in 0..N {
            src.push_str(&format!("#[a] fn f{i}() {{}}\n"));
            symbols.push(Symbol {
                name: format!("f{i}"),
                kind: "function".into(),
                line: i + 2,
                end_line: i + 2,
            });
        }
        let (docs, visited) = work::measure(|| extract(&src, "rust", &symbols));
        assert_eq!(docs.keys().collect::<Vec<_>>(), [&2], "{docs:?}");
        let bound = 4 * N;
        assert!(
            visited <= bound,
            "visited {visited} lines for {N} symbols (bound {bound}): a per-symbol rescan"
        );

        let java = "class C {\n  /** Its own. */\n  @Override public String toString() { return \"\"; }\n\
                    public void next() {}\n  /** Above its annotation. */\n  @Deprecated\n  public void old() {}\n}\n";
        let docs = docs_of(java, "java");
        let mut all: Vec<&str> = docs.values().map(String::as_str).collect();
        all.sort();
        assert_eq!(all, ["Above its annotation.", "Its own."], "{docs:?}");
        assert!(!docs.contains_key(&4), "{docs:?}");
    }

    /// A comment after an attribute or decorator, and a `]` in a char
    /// literal inside one, leave it glued to the signature below.
    #[test]
    fn a_comment_or_char_literal_after_an_attribute_keeps_the_doc() {
        let ts = "class C {\n  /** Lists. */\n  @Get() // list\n  findAll() {}\n}\n";
        let docs = docs_of(ts, "typescript");
        assert!(docs.values().any(|d| d == "Lists."), "{docs:?}");
        let rust = "/// Lexes.\n#[case(']')] /* close */\nfn lex() {}\n";
        let docs = docs_of(rust, "rust");
        assert!(docs.values().any(|d| d == "Lexes."), "{docs:?}");
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

    /// The docs the Docs view and the hover peek read: each looked for above
    /// where its declaration starts.
    fn located_docs_of(src: &str, lang: &str) -> HashMap<usize, String> {
        extract_located(src, lang, &crate::outline::extract_located(src, lang))
    }

    /// A SQL statement's doc is the run of `--` lines right above it; a
    /// blank line ends it (pg_dump's `-- Name: …; Type: TABLE` banners stand
    /// apart, and are no doc).
    #[test]
    fn a_sql_statements_doc_is_the_dash_comment_above_it() {
        let src = "-- People who act in films.\n-- One row each.\nCREATE TABLE actor (\n  -- Their given name.\n  first_name text\n);\n\n\
                   --\n-- Name: film; Type: TABLE\n--\n\nCREATE TABLE film (id int);\n";
        let docs = located_docs_of(src, "sql");
        assert_eq!(
            docs.get(&3).map(String::as_str),
            Some("People who act in films.\nOne row each.")
        );
        assert_eq!(docs.get(&5).map(String::as_str), Some("Their given name."));
        assert_eq!(docs.get(&12), None, "{docs:?}");
    }

    /// A Lean doc is the `/-- … -/` block above the declaration — above its
    /// attributes, which start it. Its lines keep their Markdown: a `*` that
    /// starts one is a bullet, not a gutter. A `/-! … -/` block documents
    /// the module, never the declaration below it.
    #[test]
    fn a_lean_doc_block_sits_above_the_attributes() {
        let src = "/-! # Sums -/\ndef zero := 0\n\n/-- Adds one.\n\n* never negative -/\n@[simp]\ntheorem succ_pos : 0 < 1 := by decide\n\
                   /-- One line. -/\nprivate def one := 1\n";
        let docs = located_docs_of(src, "lean");
        assert_eq!(docs.get(&2), None, "{docs:?}");
        assert_eq!(
            docs.get(&8).map(String::as_str),
            Some("Adds one.\n\n* never negative"),
            "{docs:?}"
        );
        assert_eq!(
            docs.get(&10).map(String::as_str),
            Some("One line."),
            "{docs:?}"
        );
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
        assert_eq!(rest_role_at(":x:`y`"), Some(("`y`".to_string(), 6)));
        assert_eq!(rest_role_at("::`y`"), None);
    }
}

/// The docs of the languages clew reads with grammars of their own.
#[cfg(test)]
mod language_tests {
    use super::*;

    fn docs_of(source: &str, lang: &str) -> Vec<(usize, String)> {
        let symbols = crate::outline::extract(source, lang);
        let mut docs: Vec<(usize, String)> = extract(source, lang, &symbols).into_iter().collect();
        docs.sort();
        docs
    }

    /// Each language's doc comments, as prose: PHPDoc's inline tags, C#'s
    /// XML (past an attribute line), Ruby's `#` runs (not its magic
    /// comment), EDoc above a `-spec`, Haddock above a signature, Zig's `///`.
    #[test]
    fn each_languages_docs_are_read_as_it_writes_them() {
        assert_eq!(
            docs_of(include_str!("../tests/fixtures/langs/a.php"), "php"),
            [
                (9, "A shop item.".into()),
                (14, "The price, with Tax.".into())
            ]
        );
        assert_eq!(
            docs_of(include_str!("../tests/fixtures/langs/a.cs"), "csharp"),
            [
                (6, "An item, see `Kind`.".into()),
                (
                    13,
                    "The price.\n\n- `tax`: The tax.\n\nReturns The `total`.".into()
                )
            ]
        );
        assert_eq!(
            docs_of(include_str!("../tests/fixtures/langs/a.rb"), "ruby"),
            [(7, "An item.".into()), (11, "The price.".into())]
        );
        assert_eq!(
            docs_of(include_str!("../tests/fixtures/langs/a.erl"), "erlang"),
            [(2, "A shop.".into()), (14, "The price.".into())]
        );
        assert_eq!(
            docs_of(include_str!("../tests/fixtures/langs/a.hs"), "haskell"),
            [
                (9, "An item.".into()),
                (16, "Things with a price.".into()),
                (24, "The price.".into())
            ]
        );
        assert_eq!(
            docs_of(include_str!("../tests/fixtures/langs/a.zig"), "zig"),
            [
                (6, "An item.".into()),
                (9, "Its cost.".into()),
                (12, "The price.".into()),
                (21, "Total.".into())
            ]
        );
        // EDoc's quoting, its tags; an apostrophe stays one.
        assert_eq!(
            render_edoc("@doc Sums `xs' — it's fast.\n@param Xs the list\n@returns the sum"),
            "Sums `xs` — it's fast.\n- Xs the list\nReturns the sum"
        );
        // A plain Haskell comment above a function is no Haddock.
        assert!(docs_of("-- just a note\nf :: Int\nf = 1\n", "haskell").is_empty());
    }
}
