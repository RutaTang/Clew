//! The API documentation index for the Docs view.
//!
//! Assembles what clew already extracts — the tree-sitter symbol outline plus
//! per-symbol doc comments — into a browsable, nested API surface for a file:
//! signature, doc, visibility, and members nested under their enclosing type.
//! No build, no language doc tool, no webview: every language clew already
//! parses. The client enriches a selected entry via LSP hover.

use std::collections::HashSet;

use clew_protocol::DocItem;

use crate::highlight::Lang;
use crate::outline::Container;

/// The kind an associated type (a Rust `type` inside an `impl` or a trait)
/// is listed under, apart from the module's own type aliases.
pub const ASSOCIATED_TYPE: &str = "associated type";

/// How deeply the emitted [`DocItem`] tree may nest before further items are
/// folded up as siblings at the cap.
///
/// Nesting is repository-controlled and was unbounded: the tree is exactly as
/// deep as the file's own symbols nest, and the server's read cap bounds the
/// BYTES it reads, not the depth — 512 KiB of `pub mod m{` nests ~30k deep.
/// Every frame crosses the transport as NDJSON — including a LOCAL project,
/// whose server is a child process over stdio — and `serde_json`'s
/// deserializer refuses more than 128 nested containers. One level costs two of
/// them (the `children` array and the child object) on top of the six the
/// `Docs` envelope already spends, so past ~60 levels the client cannot parse
/// the frame AT ALL, however small it is: it fails closed and drops the link,
/// which tears down the language servers, the debug session, the watchers and
/// every in-flight turn — and the next DOCS build repeats it. Deeper still,
/// SERIALIZING the tree overflows the server's own writer-task stack.
///
/// Same ceiling and same number as [`crate::fs_scan::MAX_TREE_DEPTH`], the
/// other recursive wire type, measured the same way (top level = 0). The two
/// must not drift apart.
pub const MAX_DOC_DEPTH: usize = 32;

/// Build the documented API of one file: top-level items, with members nested
/// under their enclosing type/module by source-range containment. Returns an
/// empty list when the language has no outline.
/// A file's own doc comment, its comment markers stripped: Rust's leading
/// `//!` lines, a Python module's docstring, Go's package comment (the
/// comment block right above `package`). Empty for the other languages,
/// whose file-top comments are licence headers as often as docs, and for a
/// file without one.
pub fn module_doc(source: &str, lang_key: &str) -> String {
    let lines = source.lines().map(|l| l.trim_end_matches('\r'));
    match lang_key {
        "rust" => {
            let mut doc: Vec<&str> = Vec::new();
            for line in lines {
                let t = line.trim_start();
                if let Some(rest) = t.strip_prefix("//!") {
                    doc.push(rest.strip_prefix(' ').unwrap_or(rest));
                } else if t.is_empty() && doc.is_empty() || t.starts_with("#!") && doc.is_empty() {
                    continue;
                } else {
                    break;
                }
            }
            doc.join("\n").trim().to_string()
        }
        "python" => {
            // The first statement, when it is a string literal.
            let body: Vec<&str> = lines
                .skip_while(|l| {
                    let t = l.trim();
                    t.is_empty() || t.starts_with('#')
                })
                .collect();
            let text = body.join("\n");
            let text = text.trim_start_matches(['r', 'R', 'u', 'U']);
            for quote in ["\"\"\"", "'''", "\"", "'"] {
                if let Some(rest) = text.strip_prefix(quote)
                    && let Some(end) = rest.find(quote)
                {
                    return rest[..end].trim().to_string();
                }
            }
            String::new()
        }
        "go" => {
            let all: Vec<&str> = lines.collect();
            let Some(pkg) = all
                .iter()
                .position(|l| l.trim_start().starts_with("package "))
            else {
                return String::new();
            };
            let mut doc: Vec<&str> = all[..pkg]
                .iter()
                .rev()
                .map(|l| l.trim_start())
                .take_while(|l| l.starts_with("//"))
                .map(|l| {
                    let rest = &l[2..];
                    rest.strip_prefix(' ').unwrap_or(rest)
                })
                .collect();
            doc.reverse();
            doc.join("\n").trim().to_string()
        }
        _ => String::new(),
    }
}

pub fn build_file(source: &str, lang_key: &str) -> Vec<DocItem> {
    let Some(lang) = Lang::for_source(lang_key, source) else {
        return Vec::new();
    };
    let mut located = crate::outline::extract_located(source, lang.key());
    if located.is_empty() {
        return Vec::new();
    }
    // Outline rows are ordered by line/name; Docs need source order even
    // within a line, with an enclosing declaration before its members.
    located.sort_by_key(|l| (l.declaration.0, std::cmp::Reverse(l.declaration.1)));
    // Docs are looked for above where each DECLARATION starts, which for a
    // GNU-style C definition is the return-type line above the name.
    let docs = crate::docs::extract_located(source, lang.key(), &located);
    let lines: Vec<&str> = source.lines().collect();

    // A flat record per symbol, in line order (outline is already sorted).
    struct Raw {
        name: String,
        kind: String,
        line: usize,
        end_line: usize,
        signature: String,
        doc: String,
        container: Option<Container>,
    }
    let raws: Vec<Raw> = located
        .iter()
        .map(|l| {
            let s = &l.symbol;
            Raw {
                name: s.name.clone(),
                // `type Item = …;` in an `impl` is a member of the impl, not
                // a type of the module: saying so keeps it out of the type
                // map and the glossary, which list the project's own types.
                kind: if lang == Lang::Rust && s.kind == "type" && l.container.is_some() {
                    ASSOCIATED_TYPE.to_string()
                } else {
                    s.kind.clone()
                },
                line: s.line,
                end_line: s.end_line,
                signature: source
                    .get(l.declaration.0..l.signature_end)
                    .unwrap_or_default()
                    .trim_end_matches([';', ','])
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                doc: docs.get(&s.line).cloned().unwrap_or_default(),
                container: l.container,
            }
        })
        .collect();

    // Nest by containment: symbol B is a child of the closest earlier symbol A
    // whose complete byte range still encloses B's declaration. A stack of
    // open ancestors gives this in one pass.
    let n = raws.len();
    let mut parent: Vec<Option<usize>> = vec![None; n];
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..n {
        while let Some(&top) = stack.last() {
            let (start, end) = located[top].declaration;
            let (child_start, child_end) = located[i].declaration;
            if start > child_start || child_start >= end || child_end > end {
                stack.pop();
            } else {
                break;
            }
        }
        // Fold past the cap rather than drop: `stack` is exactly `i`'s chain
        // of open ancestors, so `stack[d]` sits at depth `d` and attaching to
        // it puts `i` at depth `d + 1`. Past `MAX_DOC_DEPTH` we attach to the
        // deepest ancestor that still keeps `i` inside the cap, so an
        // over-deep item becomes a sibling there instead of disappearing from
        // the Docs page — losing symbols silently would be worse than the
        // unparseable frame this prevents. The index chosen is still `< i`,
        // which the bottom-up assembly below depends on, and `stack` keeps
        // every ancestor so the popping above is unaffected. The cost is that
        // a folded item's visibility is judged against its folded parent —
        // `kind_takes_members` — rather than its
        // true enclosing type, the same trade `fs_scan`'s fold makes for a
        // tree row's name.
        parent[i] = match stack.len() {
            0 => None,
            len => Some(stack[len.min(MAX_DOC_DEPTH) - 1]),
        };
        stack.push(i);
    }
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots: Vec<usize> = Vec::new();
    for (i, p) in parent.iter().enumerate() {
        match p {
            Some(p) => children[*p].push(i),
            None => roots.push(i),
        }
    }

    // Visibility needs the nesting, so it is decided here rather than with the
    // rest of each record: in the C-family languages a top-level declaration
    // and a class member follow OPPOSITE defaults, and judging both by the
    // member rule published every unexported top-level helper as public API.
    let exported = reexported_names(source, lang);

    // In index order, so a parent's verdict exists before its members'.
    let mut public: Vec<bool> = vec![false; n];
    for i in 0..n {
        // "Has a parent" is NOT "is a member". Nesting here is pure
        // source-range containment, so a function declared inside another
        // function has a parent too — and judging it by the member rule
        // ("public unless marked private") published every local helper of
        // an exported function as public API. Only a type-like enclosing
        // symbol makes its children members.
        let member_of = parent[i].filter(|&p| kind_takes_members(&raws[p].kind));
        public[i] = match (lang, raws[i].container) {
            // A trait's methods carry no `pub`: they are exactly as public as
            // the trait, which is what the member rule below cannot see.
            (Lang::Rust, Some(Container::Trait)) => member_of.is_none_or(|p| public[p]),
            // A trait implementation's methods are reachable wherever the
            // type is — they are its API through the trait, `pub` or not.
            (Lang::Rust, Some(Container::TraitImpl)) => true,
            // C++ access is section-based, so a member's own declaration line
            // says nothing about it; its type's `public:`/`private:` labels do.
            (Lang::Cpp, _) if member_of.is_some() => located[i].cpp_public.unwrap_or(true),
            _ => {
                is_public(&raws[i].signature, &raws[i].name, lang, member_of.is_some())
                    || (parent[i].is_none() && exported.contains(raws[i].name.as_str()))
            }
        };
    }

    // Assemble bottom-up rather than recursively: the nesting depth is the
    // source's, over a file the repository controls, and a recursive build
    // would overflow the stack on a deeply nested one. The containment pass
    // above pushes in increasing order, so every child's index is greater
    // than its parent's — walking indices downwards therefore always finds a
    // node's children already built.
    let mut built: Vec<Option<DocItem>> = vec![None; n];
    for i in (0..n).rev() {
        let kids = children[i]
            .iter()
            .filter_map(|&c| built[c].take())
            .collect();
        let r = &raws[i];
        let refs = if kind_takes_members(&r.kind) && r.kind != "module" {
            let spans: Vec<(usize, usize)> = children[i]
                .iter()
                .map(|&c| located[c].body.unwrap_or((raws[c].line, raws[c].end_line)))
                .collect();
            let member_signatures: Vec<&str> = children[i]
                .iter()
                .map(|&c| raws[c].signature.as_str())
                .collect();
            type_refs(
                &lines,
                &r.name,
                &r.signature,
                located[i].body,
                &spans,
                &member_signatures,
            )
        } else {
            Vec::new()
        };
        built[i] = Some(DocItem {
            name: r.name.clone(),
            kind: r.kind.clone(),
            signature: r.signature.clone(),
            doc: r.doc.clone(),
            line: r.line,
            public: public[i],
            children: kids,
            refs,
        });
    }
    roots.iter().filter_map(|&i| built[i].take()).collect()
}

/// Identifiers a type names at most (see [`type_refs`]).
pub const MAX_TYPE_REFS: usize = 400;

/// The identifiers a type's declaration, own members and member signatures
/// name — what the type map resolves against the project's types. Read from
/// the text: the declaration `signature`; the type's `body` lines with each
/// member's span (`member_spans`, 1-based inclusive) cut out, which leaves
/// the fields, variants, constants and nested declarations; and each
/// member's `signature` (a method's parameters and return type). Words a
/// language spells its syntax with are left out, as is the type's own name
/// and `Self`. In order of first sighting, without repeats, capped at
/// [`MAX_TYPE_REFS`].
fn type_refs(
    lines: &[&str],
    own_name: &str,
    signature: &str,
    body: Option<(usize, usize)>,
    member_spans: &[(usize, usize)],
    member_signatures: &[&str],
) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut take = |text: &str| {
        for word in identifiers(text) {
            if refs.len() >= MAX_TYPE_REFS {
                return;
            }
            if word == own_name || is_syntax_word(word) || seen.contains(word) {
                continue;
            }
            seen.insert(word.to_string());
            refs.push(word.to_string());
        }
    };
    take(signature);
    if let Some((first, last)) = body {
        for (i, line) in lines.iter().enumerate() {
            let line1 = i + 1;
            if line1 < first || line1 > last {
                continue;
            }
            if member_spans.iter().any(|&(a, b)| line1 >= a && line1 <= b) {
                continue;
            }
            take(line);
        }
    }
    for member in member_signatures {
        take(member);
    }
    refs
}

/// The identifier-shaped words of `text` (`[A-Za-z_][A-Za-z0-9_]*`) that can
/// name a type, in order: `//` and `#` comments are cut first, and a word is
/// left out when what follows or precedes it says it is not a type — a
/// field or parameter name (`name: Type`; a `::` path is not that), a call
/// or a tuple variant (`name(`), a member access (`.name`). A Java or Go
/// field's name (`Type name;`, `name Type`) cannot be told apart this way and
/// stays; the type map resolves every word against the project's types, so
/// it costs nothing but bytes.
fn identifiers(text: &str) -> Vec<&str> {
    let code = text.split_once("//").map_or(text, |(code, _)| code);
    let code = code.split_once('#').map_or(code, |(code, _)| code);
    let bytes = code.as_bytes();
    let mut words = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if !(c.is_ascii_alphabetic() || c == '_') {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let word = &code[start..i];
        let before = code[..start]
            .bytes()
            .rev()
            .find(|b| !b.is_ascii_whitespace());
        let after = code[i..].bytes().find(|b| !b.is_ascii_whitespace());
        let field_colon = after == Some(b':') && !code[i..].trim_start().starts_with("::");
        let called = after == Some(b'(');
        let member = before == Some(b'.');
        if !(field_colon || called || member) {
            words.push(word);
        }
    }
    words
}

/// Words that are a language's syntax or its primitive types, not a type of
/// the project: never a reference worth resolving.
fn is_syntax_word(word: &str) -> bool {
    const WORDS: &[&str] = &[
        "abstract",
        "and",
        "as",
        "async",
        "await",
        "bool",
        "boolean",
        "break",
        "byte",
        "case",
        "catch",
        "char",
        "class",
        "const",
        "constexpr",
        "continue",
        "crate",
        "def",
        "default",
        "del",
        "do",
        "double",
        "dyn",
        "elif",
        "else",
        "enum",
        "export",
        "extends",
        "extern",
        "false",
        "final",
        "finally",
        "float",
        "fn",
        "for",
        "from",
        "func",
        "function",
        "get",
        "global",
        "if",
        "impl",
        "implements",
        "import",
        "in",
        "inline",
        "instanceof",
        "int",
        "interface",
        "internal",
        "is",
        "lambda",
        "let",
        "long",
        "loop",
        "match",
        "mod",
        "mut",
        "namespace",
        "new",
        "nonlocal",
        "not",
        "null",
        "number",
        "object",
        "of",
        "operator",
        "or",
        "override",
        "package",
        "pass",
        "private",
        "protected",
        "pub",
        "public",
        "raise",
        "readonly",
        "ref",
        "return",
        "self",
        "Self",
        "set",
        "short",
        "static",
        "str",
        "string",
        "struct",
        "super",
        "switch",
        "template",
        "this",
        "throw",
        "throws",
        "trait",
        "true",
        "try",
        "type",
        "typedef",
        "typename",
        "union",
        "unsafe",
        "unsigned",
        "use",
        "using",
        "var",
        "virtual",
        "void",
        "volatile",
        "where",
        "while",
        "with",
        "yield",
        "i8",
        "i16",
        "i32",
        "i64",
        "i128",
        "isize",
        "u8",
        "u16",
        "u32",
        "u64",
        "u128",
        "usize",
        "f32",
        "f64",
        "None",
        "True",
        "False",
        "undefined",
        "any",
        "never",
        "unknown",
        "int8",
        "int16",
        "int32",
        "int64",
        "uint8",
        "uint16",
        "uint32",
        "uint64",
        "float32",
        "float64",
        "rune",
        "error",
        "nil",
        "map",
        "chan",
        "go",
        "range",
        "select",
        "defer",
        "fallthrough",
        "goto",
        "then",
    ];
    WORDS.contains(&word)
}

/// The declaration text for the item at `line1` (1-based): see
/// [`signature_from`].
#[cfg(test)]
fn signature(lines: &[&str], line1: usize) -> String {
    signature_from(lines, line1, line1)
}

/// The declaration text starting at 1-based `from`: join lines until the body
/// opens (`{`/`;`) or the signature looks complete (balanced parens and not
/// obviously continued), so multi-line signatures are captured but bodies are
/// not. Never stops before 1-based `through` — the name's line — so a
/// declaration that starts above its name (`static int` / `foo(void)`, a
/// `template <…>` line) keeps both halves.
#[cfg(test)]
fn signature_from(lines: &[&str], from: usize, through: usize) -> String {
    let start = from.saturating_sub(1);
    let must_reach = through.saturating_sub(1);
    let mut acc = String::new();
    let mut depth: i32 = 0;
    for (i, l) in lines
        .iter()
        .enumerate()
        .skip(start)
        .take(8 + must_reach.saturating_sub(start))
    {
        let cut = l.find(['{', ';']);
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
        if i >= must_reach && depth <= 0 && !t.is_empty() && !t.ends_with(',') && !t.ends_with('(')
        {
            break;
        }
    }
    acc.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a symbol of this kind gives its children the visibility defaults of
/// MEMBERS. Anything else — a function, a method, a field — encloses locals,
/// which are never part of the API.
fn kind_takes_members(kind: &str) -> bool {
    matches!(
        kind,
        "class" | "interface" | "enum" | "struct" | "trait" | "module" | "type" | "union"
    )
}

/// `decl` without a leading `template <…>` clause (balanced angle brackets).
fn without_template_clause(decl: &str) -> &str {
    let d = decl.trim_start();
    let Some(rest) = d.strip_prefix("template") else {
        return d;
    };
    let rest = rest.trim_start();
    if !rest.starts_with('<') {
        return d;
    }
    let mut depth = 0i32;
    for (i, c) in rest.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return rest[i + 1..].trim_start();
                }
            }
            _ => {}
        }
    }
    d
}

/// Longest export statement accumulated across lines. A real `export { … }`
/// clause or `module.exports = { … }` object is a few hundred lines at most;
/// past this the statement is taken as it stands.
const MAX_EXPORT_LINES: usize = 10_000;
const MAX_EXPORT_BYTES: usize = 256 * 1024;

/// A statement being accumulated across lines until its braces balance.
struct PendingExport {
    text: String,
    /// `{` minus `}` so far, kept as a running count: re-counting the whole
    /// accumulated text on every line was quadratic in the statement's length.
    depth: i64,
    lines: usize,
    /// Whether an unbalanced `{` in this statement opens something that still
    /// names exports on later lines (decided once, from the statement's head).
    continues: bool,
}

/// Names a JS/TS file exports through a separate statement rather than an
/// `export` keyword on the declaration itself (`export { a, b as c }`,
/// `export default a`, `module.exports = { a }`, `exports.a = a`). Without
/// these, requiring `export` on the declaration line would hide a genuinely
/// public API — the opposite mistake from treating every top-level helper as
/// public.
fn reexported_names(source: &str, lang: Lang) -> HashSet<String> {
    let mut out = HashSet::new();
    if !matches!(lang, Lang::TypeScript | Lang::Tsx | Lang::JavaScript) {
        return out;
    }
    // An `export { … }` clause or a `module.exports = { … }` object may span
    // lines, so accumulate until its braces balance. Only those two shapes:
    // accumulating any unbalanced line swallowed the BODY of
    // `module.exports = function () {` and published every local inside it.
    let mut pending: Option<PendingExport> = None;
    for line in source.lines() {
        let t = line.trim();
        match pending.as_mut() {
            Some(p) => {
                p.text.push(' ');
                p.text.push_str(t);
                p.depth += brace_delta(t);
                p.lines += 1;
            }
            None if opens_export_statement(t) => {
                pending = Some(PendingExport {
                    text: t.to_string(),
                    depth: brace_delta(t),
                    lines: 1,
                    continues: continues_onto_later_lines(t),
                });
            }
            None => continue,
        }
        let still_open = pending.as_ref().is_some_and(|p| {
            p.depth > 0
                && p.continues
                && p.lines < MAX_EXPORT_LINES
                && p.text.len() < MAX_EXPORT_BYTES
        });
        if still_open {
            continue;
        }
        if let Some(p) = pending.take() {
            collect_exported_names(&p.text, &mut out);
        }
    }
    // A file that ends inside the statement (a syntax error, or a truncated
    // read) still names what it names so far — dropping it hid the whole list.
    if let Some(p) = pending {
        collect_exported_names(&p.text, &mut out);
    }
    out
}

fn brace_delta(t: &str) -> i64 {
    crate::outline::work::add(t.len());
    t.matches('{').count() as i64 - t.matches('}').count() as i64
}

/// Whether this line starts a statement that can name exports.
fn opens_export_statement(t: &str) -> bool {
    t.starts_with("export {")
        || t.starts_with("export type {")
        || t.starts_with("export default ")
        || t.starts_with("exports.")
        || t.starts_with("module.exports")
}

/// Whether an unbalanced `{` in this statement opens something that still
/// names exports on later lines. An export clause always does; a CommonJS
/// assignment does only in its object form. Treating every unbalanced line as
/// continuable swallowed the BODY of `module.exports = function () {` and
/// published every local declared inside it.
fn continues_onto_later_lines(statement: &str) -> bool {
    statement.starts_with("export {")
        || statement.starts_with("export type {")
        || statement
            .split_once('=')
            .is_some_and(|(_, rhs)| rhs.trim_start().starts_with('{'))
}

/// Add every LOCAL declaration `statement` publishes to `out`.
fn collect_exported_names(statement: &str, out: &mut HashSet<String>) {
    // CommonJS first: `exports.a = b` also starts with "export".
    if statement.starts_with("module.exports") || statement.starts_with("exports.") {
        collect_commonjs_names(statement, out);
        return;
    }
    if let Some(rest) = statement.strip_prefix("export default ") {
        // `export default function f` carries the keyword on the declaration,
        // where `is_public` already sees it; only the bare-identifier form
        // needs naming here.
        let name = rest.trim().trim_end_matches(';').trim();
        if is_plain_ident(name) {
            out.insert(name.to_string());
        }
        return;
    }
    // `export { … }`. `export { a } from "./dep"` re-exports somebody ELSE's
    // names. A local declaration that happens to share one is not exported by
    // it, and neither is a local named after a path segment of the specifier.
    let Some(open) = statement.find('{') else {
        return;
    };
    // No closing brace: the file ended inside the clause, which still names
    // what it names so far.
    let close = statement
        .rfind('}')
        .filter(|&close| close > open)
        .unwrap_or(statement.len());
    if statement[close..].contains(" from ") {
        return;
    }
    for clause in statement[open + 1..close].split(',') {
        // `a as b` publishes the LOCAL `a` under the name `b`; `b` names
        // nothing in this file.
        let local = clause.split(" as ").next().unwrap_or(clause).trim();
        let local = local.trim_start_matches("type ").trim();
        if is_plain_ident(local) {
            out.insert(local.to_string());
        }
    }
}

/// The locals a CommonJS export statement publishes:
///   `exports.a = b` / `module.exports.a = b` → `a` (the name the outline
///   gives a function assigned there) and the local `b`;
///   `module.exports = b` / `= function b () {…}` / `= class B {…}` → `b`;
///   `module.exports = { a, b: c, d() {…}, e: () => … }` → `a`, `c`, `d`, `e`.
///
/// Only the object's TOP-LEVEL entries count. Taking every identifier in the
/// statement published the private helpers that exported functions merely
/// CALL (`module.exports = { run() { helper(); } }` made `helper` public).
fn collect_commonjs_names(statement: &str, out: &mut HashSet<String>) {
    let Some((lhs, rhs)) = statement.split_once('=') else {
        return;
    };
    let lhs = lhs.trim();
    let rhs = rhs.trim().trim_end_matches(';').trim();
    // `exports.a` / `module.exports.a`: the member name itself.
    let member = lhs
        .strip_prefix("module.exports.")
        .or_else(|| lhs.strip_prefix("exports."));
    if let Some(name) = member
        && is_plain_ident(name)
    {
        out.insert(name.to_string());
    }
    if let Some(body) = rhs.strip_prefix('{') {
        collect_object_entries(body, out);
        return;
    }
    if let Some(name) = declared_name(rhs) {
        out.insert(name.to_string());
    }
}

/// `b` for a right-hand side that is a plain identifier `b`, a named function
/// `function b (…) {…}` / `async function b`, or a named class `class B {…}`.
fn declared_name(rhs: &str) -> Option<&str> {
    if is_plain_ident(rhs) {
        return Some(rhs);
    }
    let rest = rhs.strip_prefix("async ").unwrap_or(rhs).trim_start();
    let rest = rest
        .strip_prefix("function")
        .map(|r| r.trim_start_matches('*'))
        .or_else(|| rest.strip_prefix("class "))?
        .trim_start();
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(rest.len());
    let name = &rest[..end];
    is_plain_ident(name).then_some(name)
}

/// The top-level entries of an object literal whose text starts right after
/// its `{`, stopping at the matching `}`.
fn collect_object_entries(body: &str, out: &mut HashSet<String>) {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start = 0usize;
    let mut entries: Vec<&str> = Vec::new();
    let mut end = body.len();
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        if let Some(q) = quote {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                _ if c == q => quote = None,
                _ => {}
            }
            continue;
        }
        match c {
            '"' | '\'' | '`' => quote = Some(c),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' => depth -= 1,
            '}' if depth == 0 => {
                end = i;
                break;
            }
            '}' => depth -= 1,
            ',' if depth == 0 => {
                entries.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < end {
        entries.push(&body[start..end]);
    }
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() || entry.starts_with("...") {
            continue;
        }
        // `key: value` — the key when the value defines a function there, the
        // local when the value names one.
        if let Some((key, value)) = split_entry(entry) {
            let key = key.trim().trim_matches(['"', '\'']);
            let value = value.trim();
            let defines_function =
                value.starts_with("function") || value.starts_with("async") || value.contains("=>");
            if defines_function && is_plain_ident(key) {
                out.insert(key.to_string());
            } else if let Some(name) = declared_name(value) {
                out.insert(name.to_string());
            }
            continue;
        }
        // Method shorthand `name(…) {…}` (also `async name`, `get name`,
        // `*name`), or a shorthand property `name`.
        let head = entry
            .trim_start_matches("async ")
            .trim_start_matches("get ")
            .trim_start_matches("set ")
            .trim_start_matches('*')
            .trim_start();
        let name_end = head
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
            .unwrap_or(head.len());
        let name = &head[..name_end];
        if is_plain_ident(name) {
            out.insert(name.to_string());
        }
    }
}

/// `(key, value)` for a `key: value` entry — split at the first `:` outside
/// any brackets, so a method's type annotation is not mistaken for one.
fn split_entry(entry: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    for (i, c) in entry.char_indices() {
        match c {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth -= 1,
            ':' if depth == 0 => return Some((&entry[..i], &entry[i + 1..])),
            _ => {}
        }
        // A method shorthand's parameter list opens before any key colon.
        if c == '(' && depth == 1 {
            return None;
        }
    }
    None
}

/// Whether `s` is a single JS identifier and nothing else.
fn is_plain_ident(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// Whether the item is part of the public API, per each language's convention.
/// `decl` is its declaration text (its signature), `name` its identifier, and
/// `is_member` says whether it is nested inside a type-like item (a class
/// member) rather than declared at the top level of the file.
fn is_public(decl: &str, name: &str, lang: Lang, is_member: bool) -> bool {
    let d = decl.trim_start();
    match lang {
        // Any `pub` (including pub(crate)/pub(super)) counts for the surface.
        Lang::Rust => d.starts_with("pub"),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            if is_member {
                // Class members are public unless they say otherwise.
                !(d.contains("private ") || name.starts_with('#'))
            } else {
                // A top-level declaration is reachable only if it is
                // EXPORTED. Applying the member default here — public unless
                // marked private, which has no meaning at file scope — put
                // every internal helper in the default public-only Docs view.
                d.starts_with("export") || d.starts_with("module.exports")
            }
        }
        // Exported = capitalized identifier.
        Lang::Go => name.chars().next().is_some_and(char::is_uppercase),
        // Convention: a leading underscore marks non-public — except a
        // dunder (`__init__`, `__call__`, `__eq__`), which is the most public
        // thing a Python class has.
        Lang::Python => {
            !name.starts_with('_')
                || (name.len() > 4 && name.starts_with("__") && name.ends_with("__"))
        }
        Lang::Dart => !name.starts_with('_'),
        // Java says it outright, in any modifier order. Package-private (no
        // modifier at all) counts as public here: it is the default for a lot
        // of ordinary API, and calling it private would empty the Docs view
        // of most files.
        Lang::Java => !modifiers(d).any(|w| w == "private" || w == "protected"),
        // C and C++: a top-level definition marked `static` has internal
        // linkage, so it is not part of the file's API. C++ MEMBERS never get
        // here — their access comes from their type's sections, decided in
        // `build_file`.
        Lang::C | Lang::Cpp => !modifiers(without_template_clause(d)).any(|w| w == "static"),
        // No outline, so no items to judge; the benefit of the doubt keeps a
        // Docs view from coming up empty if one ever appears.
        Lang::Json | Lang::Bash | Lang::Yaml | Lang::Toml | Lang::Html | Lang::Css | Lang::Zig => {
            true
        }
    }
}

/// The words before a declaration's parameter list — where every language in
/// the C family puts its visibility and linkage keywords.
fn modifiers(decl: &str) -> impl Iterator<Item = &str> {
    decl.split('(').next().unwrap_or(decl).split_whitespace()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A type's `refs`: its declaration's names, its own members' (fields,
    /// variants) with the methods' bodies cut out, and its methods'
    /// signatures — not the syntax words, not itself, each once, in order.
    /// A function has none.
    #[test]
    fn a_types_refs_name_what_its_declaration_and_members_use() {
        let src = "pub struct Order<T: Clone> {\n    pub customer: Customer,\n    lines: Vec<OrderLine>,\n\
                   }\n\nimpl Order<u8> {\n    pub fn total(&self, rates: &TaxRates) -> Money {\n        let x: Discount = local();\n        x.into()\n    }\n}\n\
                   pub enum Status {\n    Open,\n    Shipped(Shipment),\n}\npub fn helper(c: Customer) {}\n";
        let items = build_file(src, "rust");
        let find = |name: &str| items.iter().find(|i| i.name == name).expect(name);
        assert_eq!(
            find("Order").refs,
            ["Clone", "Customer", "Vec", "OrderLine"],
            "{:?}",
            find("Order").refs
        );
        // A unit variant stays (nothing on the line says it is not a type);
        // the map resolves it against the project's types and drops it.
        assert_eq!(find("Status").refs, ["Open", "Shipment"]);
        assert!(find("helper").refs.is_empty());
        // A class: field declarations and method signatures, the method
        // bodies' locals left out; the bases too.
        let src = "class Cart extends Base implements Priced, Serializable {\n  private items: Item[] = [];\n\
                   owner: Customer;\n  total(rates: TaxRates): Money {\n    const tmp: Scratch = compute();\n    return tmp;\n  }\n}\n";
        let items = build_file(src, "typescript");
        let cart = items.iter().find(|i| i.name == "Cart").expect("Cart");
        assert_eq!(
            cart.refs,
            [
                "Base",
                "Priced",
                "Serializable",
                "Item",
                "Customer",
                "TaxRates",
                "Money"
            ],
            "{:?}",
            cart.refs
        );
        assert!(
            !cart.refs.iter().any(|r| r == "Scratch"),
            "a method body's local"
        );
    }

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
        assert!(
            cls.children
                .iter()
                .any(|c| c.name == "_secret" && !c.public)
        );
    }

    #[test]
    fn signature_stops_at_body() {
        let lines = vec!["pub fn add(a: i32, b: i32) -> i32 {", "    a + b", "}"];
        assert_eq!(signature(&lines, 1), "pub fn add(a: i32, b: i32) -> i32");
    }
}

#[cfg(test)]
mod depth_tests {
    use super::*;

    /// Deepest nesting level in `items`, counting top-level items as 0, and how
    /// many items the tree holds. Walked with an explicit stack: an over-deep
    /// tree is exactly what this module is about, so the CHECK must not be the
    /// thing that overflows.
    fn depth_and_count(items: &[DocItem]) -> (usize, usize) {
        let (mut deepest, mut count) = (0usize, 0usize);
        let mut todo: Vec<(usize, &DocItem)> = items.iter().map(|i| (0usize, i)).collect();
        while let Some((depth, item)) = todo.pop() {
            deepest = deepest.max(depth);
            count += 1;
            for c in &item.children {
                todo.push((depth + 1, c));
            }
        }
        (deepest, count)
    }

    /// The real frame the DOCS tab receives, as the client parses it back.
    fn round_trips(items: Vec<DocItem>) -> Result<(), serde_json::Error> {
        let msg = clew_protocol::ServerMessage::Reply {
            id: 1,
            event: clew_protocol::Event::Docs {
                root: "/p".to_string(),
                files: vec![clew_protocol::DocFile {
                    rel: "deep.rs".to_string(),
                    items,
                    doc: String::new(),
                }],
            },
        };
        let line = serde_json::to_string(&msg).unwrap();
        serde_json::from_str::<clew_protocol::ServerMessage>(&line).map(|_| ())
    }

    /// A deeply nested file must still produce a Docs frame the CLIENT can
    /// parse. The frame here is a few KB — what this guards is `serde_json`'s
    /// 128-container recursion limit, not any byte cap — and it is fatal: an
    /// unparseable frame makes the client drop the whole transport, taking the
    /// language servers, the debug session and every in-flight turn with it,
    /// and each retry does it again. Mirrors
    /// `fs_scan::deep_nesting_stays_within_the_wire_recursion_limit` for the
    /// other recursive wire type.
    #[test]
    fn deep_nesting_stays_within_the_wire_recursion_limit() {
        const DEPTH: usize = 200; // comfortably past MAX_DOC_DEPTH
        let mut src = String::new();
        for i in 0..DEPTH {
            src.push_str(&format!("pub mod m{i} {{\n"));
        }
        for _ in 0..DEPTH {
            src.push_str("}\n");
        }
        let symbols = crate::outline::extract(&src, "rust");
        assert_eq!(symbols.len(), DEPTH, "the outline itself lost modules");

        let items = build_file(&src, "rust");
        let (deepest, count) = depth_and_count(&items);
        // The assertion that was missing: the real wire envelope round-trips.
        let back = round_trips(items);
        assert!(
            back.is_ok(),
            "the client cannot parse its own Docs frame: {:?}",
            back.err()
        );
        assert!(
            deepest <= MAX_DOC_DEPTH,
            "docs nest {deepest} levels, past the cap"
        );
        // Folded, not dropped: every symbol still has a row on the Docs page.
        assert_eq!(count, DEPTH, "folding lost {} items", DEPTH - count);
    }

    /// Minified peers must remain peers even though they share a source line.
    #[test]
    fn a_minified_one_liner_does_not_chain_past_the_cap() {
        const N: usize = 200;
        let src: String = (0..N).map(|i| format!("function f{i}(){{}}")).collect();
        let items = build_file(&src, "javascript");
        let (deepest, count) = depth_and_count(&items);
        let back = round_trips(items);
        assert!(
            back.is_ok(),
            "the client cannot parse its own Docs frame: {:?}",
            back.err()
        );
        assert_eq!(deepest, 0, "independent functions were nested together");
        assert_eq!(count, N, "folding lost {} items", N - count);
    }
}

#[cfg(test)]
mod visibility_tests {
    use super::*;

    fn find<'a>(items: &'a [DocItem], name: &str) -> &'a DocItem {
        items
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("no item {name} in {items:?}"))
    }

    /// A top-level declaration and a class member follow opposite defaults.
    /// Judging both by the member rule ("public unless marked private") put
    /// every unexported helper into the default public-only Docs view.
    #[test]
    fn top_level_js_needs_export_but_members_do_not() {
        let src = "\
export function shown() {}
function internalOnly() {}
export class Widget {
  render() {}
  private hidden() {}
}
";
        let items = build_file(src, "typescript");
        assert!(find(&items, "shown").public);
        assert!(!find(&items, "internalOnly").public, "{items:?}");

        let widget = find(&items, "Widget");
        assert!(widget.public);
        assert!(find(&widget.children, "render").public);
        assert!(!find(&widget.children, "hidden").public);
    }

    /// Exporting through a separate statement still counts, so requiring the
    /// keyword on the declaration does not hide a real public API.
    #[test]
    fn a_separate_export_statement_marks_the_declaration_public() {
        let src = "\
function alpha() {}
function beta() {}
function unexported() {}
export { alpha, beta as renamed };
";
        let items = build_file(src, "javascript");
        assert!(find(&items, "alpha").public);
        assert!(find(&items, "beta").public);
        assert!(!find(&items, "unexported").public, "{items:?}");
    }

    /// The other two ways a JS file names an existing declaration as its API.
    /// Missing them hid a module's real entry point from the Docs view.
    #[test]
    fn default_and_commonjs_exports_count() {
        let src = "\
function helper() {}
export default helper;
function shipped() {}
exports.shipped = shipped;
function listed() {}
module.exports = {
  listed
};
";
        let items = build_file(src, "javascript");
        for name in ["helper", "shipped", "listed"] {
            assert!(find(&items, name).public, "{name} in {items:?}");
        }
    }

    /// `export { … } from "…"` re-exports another module's names, so a local
    /// declaration that merely shares one is NOT part of this file's API —
    /// and neither is one named after a word in the specifier.
    #[test]
    fn a_re_export_does_not_publish_unrelated_locals() {
        let src = "\
function outer() {}
function from() {}
function dep() {}
export { outer } from \"./dep\";
";
        let items = build_file(src, "javascript");
        for name in ["outer", "from", "dep"] {
            assert!(!find(&items, name).public, "{name} in {items:?}");
        }
    }

    /// An `as` rename publishes the LOCAL name. The alias names nothing in
    /// this file, so a local that happens to match it stays private.
    #[test]
    fn an_alias_does_not_publish_a_local_of_the_same_name() {
        let src = "\
function beta() {}
function renamed() {}
export { beta as renamed };
";
        let items = build_file(src, "javascript");
        assert!(find(&items, "beta").public);
        assert!(!find(&items, "renamed").public, "{items:?}");
    }

    /// `module.exports = function () {` is not an object literal, so the
    /// scan must stop at that line instead of swallowing the body and
    /// publishing every local inside it.
    #[test]
    fn a_commonjs_function_body_is_not_swallowed() {
        let src = "\
function hidden() {}
module.exports = function () {
  return hidden();
};
";
        let items = build_file(src, "javascript");
        assert!(!find(&items, "hidden").public, "{items:?}");
    }

    /// Nesting is source-range containment, so a local declared inside a
    /// function has a parent. Judging it by the CLASS-MEMBER rule published
    /// every helper of an exported function as public API.
    #[test]
    fn locals_of_an_exported_function_are_not_members() {
        let src = "\
export function outer() {
  function inner() {}
  return inner;
}
";
        let items = build_file(src, "typescript");
        let outer = find(&items, "outer");
        assert!(outer.public);
        assert!(!find(&outer.children, "inner").public, "{items:?}");
    }

    /// A C++ file exercising every input the section fold walks over:
    /// both type defaults, all three access labels, a label AFTER the member it
    /// does not govern, a nested type whose labels must not leak out to the
    /// enclosing class, a same-line `{ }` pair, and braces inside a body.
    const CPP_SAMPLE: &str = "\
class Outer {
  void implicit_private();
public:
  void shown();
  struct Inner {
    void inner_default();
  private:
    void inner_hidden();
  };
  void after_nested() { if (x) { y(); } }
protected:
  void guarded();
private:
  void hidden();
};

struct S {
  void s_default();
private:
  void s_hidden();
public:
  void s_back();
};
";

    /// Every item's dotted path and public flag, in document order. Walked with
    /// an explicit stack for the same reason `build_file` assembles with one:
    /// the nesting depth belongs to the source, not to this test.
    fn flat(items: &[DocItem]) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        let mut todo: Vec<(String, &DocItem)> =
            items.iter().rev().map(|i| (String::new(), i)).collect();
        while let Some((prefix, item)) = todo.pop() {
            let path = format!("{prefix}{}", item.name);
            out.push((path.clone(), item.public));
            let nested = format!("{path}.");
            for c in item.children.iter().rev() {
                todo.push((nested.clone(), c));
            }
        }
        out
    }

    /// Pinned against the output of the per-member rescan, captured before the
    /// fold was carried forward across members. That rewrite is a pure
    /// performance change, so every verdict here must stay exactly as it was.
    #[test]
    fn cpp_visibility_is_exactly_what_the_per_member_scan_produced() {
        let got = flat(&build_file(CPP_SAMPLE, "cpp"));
        let want: Vec<(String, bool)> = [
            ("Outer", true),
            ("Outer.implicit_private", false),
            ("Outer.shown", true),
            ("Outer.Inner", true),
            ("Outer.Inner.inner_default", true),
            ("Outer.Inner.inner_hidden", false),
            // The nested type's `private:` must not leak back out to Outer.
            ("Outer.after_nested", true),
            ("Outer.guarded", false),
            ("Outer.hidden", false),
            ("S", true),
            ("S.s_default", true),
            ("S.s_hidden", false),
            ("S.s_back", true),
        ]
        .into_iter()
        .map(|(n, p)| (n.to_string(), p))
        .collect();
        assert_eq!(got, want);
    }

    /// One class, many members. Deciding each member's section by rescanning
    /// from the class declaration is quadratic in the member count, with no cap
    /// on either side: a 512 KiB header of this shape measured at ~11 s in
    /// release. The fold visits each line of the class once no matter how many
    /// members read it — asserted on the lines folded (a restart per member
    /// folds ~N²/2 = 72M here), not on a wall clock. Output is asserted too,
    /// so a faster wrong answer fails.
    #[test]
    fn a_class_with_many_members_is_not_rescanned_per_member() {
        const N: usize = 12_000;
        let mut src = String::from("class K {\n");
        for i in 0..N {
            // Half the members sit after a `private:` label, so the assertion
            // below pins the section fold and not just "it returned".
            if i == N / 2 {
                src.push_str("private:\n");
            }
            src.push_str(&format!("  int m{i}();\n"));
        }
        src.push_str("};\n");

        let (items, folded) = crate::outline::work::measure(|| build_file(&src, "cpp"));

        let k = find(&items, "K");
        assert_eq!(k.children.len(), N, "outline lost members");
        let public = k.children.iter().filter(|c| c.public).count();
        // `class` starts private, so nothing before the label is public either.
        assert_eq!(public, 0, "section fold changed its verdicts");
        // Every pass `build_file` counts (the section fold, the doc index) is
        // one pass over the lines; a restart per member is thousands.
        let lines = src.lines().count();
        assert!(
            folded <= 4 * lines,
            "scanned {folded} lines of a {lines}-line class: a rescan per member"
        );
    }

    /// Java, C and C++ all say what is private; answering `true` for every
    /// language without a special case published all of it.
    #[test]
    fn c_family_private_symbols_are_not_public() {
        let java = build_file(
            "public class A {\n  private void hidden() {}\n  public void shown() {}\n}\n",
            "java",
        );
        let a = find(&java, "A");
        assert!(a.public);
        assert!(!find(&a.children, "hidden").public, "{java:?}");
        assert!(find(&a.children, "shown").public, "{java:?}");

        let c = build_file(
            "static int hidden(void) { return 1; }\nint shown(void) { return 2; }\n",
            "c",
        );
        assert!(!find(&c, "hidden").public, "{c:?}");
        assert!(find(&c, "shown").public, "{c:?}");

        // C++ access is section-based, and `class` starts out private.
        let cpp = build_file(
            "class A {\n  void implicitly_private();\npublic:\n  void shown();\nprivate:\n  void hidden();\n};\n",
            "cpp",
        );
        let a = find(&cpp, "A");
        assert!(!find(&a.children, "implicitly_private").public, "{cpp:?}");
        assert!(find(&a.children, "shown").public, "{cpp:?}");
        assert!(!find(&a.children, "hidden").public, "{cpp:?}");

        // A `struct` starts out public.
        let cpp = build_file(
            "struct B {\n  void shown();\nprivate:\n  void hidden();\n};\n",
            "cpp",
        );
        let b = find(&cpp, "B");
        assert!(find(&b.children, "shown").public, "{cpp:?}");
        assert!(!find(&b.children, "hidden").public, "{cpp:?}");
    }
}

#[cfg(test)]
mod surface_tests {
    use super::*;

    fn find<'a>(items: &'a [DocItem], name: &str) -> &'a DocItem {
        items
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("no item {name} in {items:?}"))
    }

    #[test]
    fn same_line_declarations_keep_their_own_signatures_and_parents() {
        let items = build_file("pub fn z() {} pub fn a() {}", "rust");
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].name, "z");
        assert_eq!(items[0].signature, "pub fn z()");
        assert_eq!(items[1].name, "a");
        assert_eq!(items[1].signature, "pub fn a()");
        assert!(items.iter().all(|i| i.children.is_empty()));

        let items = build_file(
            "export class Z { private x(): void {} public y(): void {} } export class A {}",
            "typescript",
        );
        assert_eq!(items.len(), 2, "{items:?}");
        let z = find(&items, "Z");
        assert_eq!(z.signature, "export class Z");
        assert_eq!(z.children.len(), 2, "{items:?}");
        let x = find(&z.children, "x");
        let y = find(&z.children, "y");
        assert_eq!(x.signature, "private x(): void");
        assert_eq!(y.signature, "public y(): void");
        assert!(!x.public);
        assert!(y.public);
        assert!(x.children.is_empty() && y.children.is_empty());

        let items = build_file(
            "export class A { run(): void {} } export class B { run(): void {} }",
            "typescript",
        );
        assert_eq!(items.len(), 2, "{items:?}");
        for class in ["A", "B"] {
            assert_eq!(find(&items, class).children.len(), 1, "{items:?}");
            assert_eq!(find(&items, class).children[0].name, "run");
        }
    }

    #[test]
    fn declaration_signatures_keep_literal_punctuation_and_multiline_parameters() {
        let items = build_file("def f(\n    x=\"{;\",\n    y=1,\n):\n    pass\n", "python");
        assert_eq!(find(&items, "f").signature, "def f( x=\"{;\", y=1, ):");
        let items = build_file("pub fn f(\n    x: i32,\n) -> i32 { x }\n", "rust");
        assert_eq!(find(&items, "f").signature, "pub fn f( x: i32, ) -> i32");
        let items = build_file("pub struct X(u32);", "rust");
        assert_eq!(find(&items, "X").signature, "pub struct X(u32)");
        let items = build_file("/** API docs. */\nexport\nfunction f() {}", "typescript");
        assert_eq!(find(&items, "f").signature, "export function f()");
        assert_eq!(find(&items, "f").doc, "API docs.");
        assert!(find(&items, "f").public);
        let items = build_file("export;\nfunction f() {}", "typescript");
        assert_eq!(find(&items, "f").signature, "function f()");
        assert!(!find(&items, "f").public);
    }

    #[test]
    fn cpp_access_ignores_comments_and_literals_and_reads_same_line_labels() {
        for noise in [
            "// An opening brace: {",
            "/* An opening brace: {\n another { public: */",
            "const char *s = \"{\";",
            "const char *s = R\"tag({ private: })tag\";",
            "char c = '{';",
        ] {
            let src = format!(
                "class A {{\n{noise}\npublic: void shown(); private: void hidden();\n}};\n"
            );
            let items = build_file(&src, "cpp");
            let a = find(&items, "A");
            assert!(find(&a.children, "shown").public, "{src}\n{items:?}");
            assert!(!find(&a.children, "hidden").public, "{src}\n{items:?}");
            assert_eq!(find(&a.children, "shown").signature, "void shown()");
        }
    }

    #[test]
    fn cpp_access_reads_preprocessor_labels_without_leaking_nested_access() {
        let items = build_file(
            "class A {\n#ifdef OPTIONAL\npublic:\n void optional();\n#else\nprivate:\n void alternative();\n#endif\n void after();\n};\nstruct B {\n#if 1\n class Inner { private: void inner(); };\n void shown();\n#endif\n};\n",
            "cpp",
        );
        let a = find(&items, "A");
        assert!(find(&a.children, "optional").public, "{items:?}");
        assert!(!find(&a.children, "alternative").public, "{items:?}");
        assert!(!find(&a.children, "after").public, "{items:?}");
        let b = find(&items, "B");
        assert!(find(&b.children, "shown").public, "{items:?}");
        assert!(
            !find(&find(&b.children, "Inner").children, "inner").public,
            "{items:?}"
        );
    }

    /// A trait's methods carry no `pub` and are exactly as public as the
    /// trait; a trait implementation's methods are part of the type's API.
    #[test]
    fn rust_trait_methods_follow_the_trait() {
        let src = "\
pub trait Shape {
    fn describe(&self) -> String { String::new() }
}
trait Hidden {
    fn secret(&self) {}
}
pub struct Sq;
impl Shape for Sq {
    fn describe(&self) -> String { \"sq\".into() }
}
impl Sq {
    fn helper(&self) {}
    pub fn new() -> Sq { Sq }
}
";
        let items = build_file(src, "rust");
        let shape = find(&items, "Shape");
        assert!(shape.public);
        assert!(find(&shape.children, "describe").public, "{items:?}");
        let hidden = find(&items, "Hidden");
        assert!(!find(&hidden.children, "secret").public, "{items:?}");
        let impl_describe = items
            .iter()
            .find(|i| i.name == "describe" && i.line == 9)
            .expect("the trait impl's method is an item");
        assert!(impl_describe.public, "{items:?}");
        assert!(!find(&items, "helper").public);
        assert!(find(&items, "new").public);
    }

    /// GNU style puts the return type (and `static`) on the line ABOVE the
    /// name. The item's signature, visibility and doc all start there.
    #[test]
    fn gnu_style_c_definitions_keep_their_first_line() {
        let src = "\
/** Public API. */
int
api_call(int x)
{
  return helper(x);
}

/** Internal. */
static int
helper(int x)
{
  return x;
}
";
        let items = build_file(src, "c");
        let api = find(&items, "api_call");
        assert!(api.public);
        assert_eq!(api.signature, "int api_call(int x)");
        assert_eq!(api.doc, "Public API.");
        let helper = find(&items, "helper");
        assert!(!helper.public, "a static definition is internal: {items:?}");
        assert_eq!(helper.signature, "static int helper(int x)");
        assert_eq!(helper.doc, "Internal.");
    }

    /// A one-line `template <…> class` is still a class, private by default.
    #[test]
    fn a_template_clause_does_not_make_a_class_default_public() {
        let src =
            "template <typename T> class Box {\n  void hidden();\npublic:\n  void shown();\n};\n";
        let items = build_file(src, "cpp");
        let b = find(&items, "Box");
        assert!(!find(&b.children, "hidden").public, "{items:?}");
        assert!(find(&b.children, "shown").public, "{items:?}");
        assert_eq!(
            without_template_clause("template <class A, class B<C>> struct S"),
            "struct S"
        );
        assert_eq!(without_template_clause("class K"), "class K");
    }

    /// Only an exported object's TOP-LEVEL entries are exported; a helper the
    /// exported method merely calls is not.
    #[test]
    fn commonjs_object_exports_publish_their_entries_only() {
        let src = "\
function helper() {}
function helper2() {}
function shorthand() {}
function internal() {}
module.exports = {
  run() { helper(); internal(); },
  other: helper2,
  shorthand,
  arrow: () => internal(),
};
";
        let items = build_file(src, "javascript");
        for name in ["helper2", "shorthand", "run", "arrow"] {
            assert!(find(&items, name).public, "{name} in {items:?}");
        }
        for name in ["helper", "internal"] {
            assert!(!find(&items, name).public, "{name} leaked: {items:?}");
        }

        let src = "function helper() {}\nexports.run = function () { return helper(); };\n";
        let items = build_file(src, "javascript");
        assert!(find(&items, "run").public, "{items:?}");
        assert!(!find(&items, "helper").public, "{items:?}");
    }

    /// A file that ends inside an export clause still exports what it names.
    #[test]
    fn an_unterminated_export_clause_at_eof_still_counts() {
        let src = "function alpha() {}\nfunction beta() {}\nexport {\n  alpha,\n  beta";
        let items = build_file(src, "javascript");
        assert!(find(&items, "alpha").public, "{items:?}");
        assert!(find(&items, "beta").public, "{items:?}");
    }

    /// Accumulating a long export clause is linear: the running brace count
    /// replaced a re-count of the whole accumulated text on every line — which
    /// scanned ~N²/2 lines' worth of text (≈800M bytes here). Asserted on the
    /// bytes brace-counted, not on a wall clock.
    #[test]
    fn a_long_export_clause_is_linear() {
        // Inside MAX_EXPORT_LINES, so the whole clause is read.
        const N: usize = 9_000;
        let mut src = String::from("export {\n");
        for i in 0..N {
            src.push_str(&format!("  name_number_{i},\n"));
        }
        src.push_str("};\n");
        let (names, counted) =
            crate::outline::work::measure(|| reexported_names(&src, Lang::JavaScript));
        assert_eq!(names.len(), N);
        assert!(names.contains(&format!("name_number_{}", N - 1)));
        assert!(
            counted <= src.len(),
            "brace-counted {counted} bytes of a {}-byte file",
            src.len()
        );
    }

    /// Dunder methods are Python's public protocol, not private helpers.
    #[test]
    fn python_dunders_are_public() {
        let src = "class K:\n    def __init__(self):\n        pass\n    def __repr__(self):\n        pass\n    def _private(self):\n        pass\n    def __mangled(self):\n        pass\n";
        let items = build_file(src, "python");
        let k = find(&items, "K");
        assert!(find(&k.children, "__init__").public);
        assert!(find(&k.children, "__repr__").public);
        assert!(!find(&k.children, "_private").public);
        assert!(!find(&k.children, "__mangled").public);
    }

    #[test]
    fn a_files_own_doc_is_read_per_language() {
        assert_eq!(
            module_doc(
                "//! Debug Adapter Protocol (DAP) support.\n//!\n//! More.\nuse x;\n",
                "rust"
            ),
            "Debug Adapter Protocol (DAP) support.\n\nMore."
        );
        assert_eq!(module_doc("#![allow(x)]\nuse y;\n", "rust"), "");
        assert_eq!(
            module_doc("/// An item's doc, not the file's.\nfn f() {}\n", "rust"),
            ""
        );
        assert_eq!(
            module_doc(
                "#!/usr/bin/env python\n# coding: utf-8\n\n\"\"\"The catalog.\n\nMore.\"\"\"\nimport x\n",
                "python"
            ),
            "The catalog.\n\nMore."
        );
        assert_eq!(module_doc("'''Orders.'''\n", "python"), "Orders.");
        assert_eq!(
            module_doc("import x\n\"\"\"Not first.\"\"\"\n", "python"),
            ""
        );
        assert_eq!(
            module_doc(
                "// Copyright.\n\n// Package shop prices orders.\n// Carts too.\npackage shop\n",
                "go"
            ),
            "Package shop prices orders.\nCarts too."
        );
        assert_eq!(
            module_doc("/** Licence. */\nexport const a = 1;\n", "typescript"),
            ""
        );
    }
}
