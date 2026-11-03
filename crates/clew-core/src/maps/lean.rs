//! Lean 4's maps, read line by line (the grammar is too loose for a tree
//! to name declarations reliably):
//!
//! - **theorem dependencies** — each theorem, and the project's theorems,
//!   definitions and structures its statement and proof use, resolved
//!   through `namespace`s and `open`s;
//! - **notation and simp** — each notation (`infixl " +++ " => f`,
//!   `notation a " ≈ " b => R a b`) and what it means; each `@[simp]` lemma
//!   (or `attribute [simp] x`) and the notation or definition its left-hand
//!   side rewrites.

use std::collections::{HashMap, HashSet};

use clew_protocol::ProjectMap;

use super::{Corpus, MapBuilder, Source, line_at};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let files: Vec<File> = corpus.in_langs(&[Lang::Lean]).map(File::read).collect();
    let decls: Vec<&Decl> = files.iter().flat_map(|f| &f.decls).collect();
    let index: HashMap<&str, usize> = decls
        .iter()
        .enumerate()
        .map(|(i, d)| (d.full.as_str(), i))
        .collect();
    let mut out = Vec::new();
    out.extend(theorems(&files, &decls, &index));
    out.extend(notation(&files, &decls, &index));
    out
}

/// `text` with comments blanked (offsets kept), and strings too when
/// `strings` is set.
fn blank(text: &str, strings: bool) -> String {
    let b = text.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    let wipe = |out: &mut Vec<u8>, from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    while i < b.len() {
        if b[i] == b'-' && b.get(i + 1) == Some(&b'-') {
            let end = b[i..]
                .iter()
                .position(|c| *c == b'\n')
                .map_or(b.len(), |p| i + p);
            wipe(&mut out, i, end);
            i = end;
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'-') {
            let start = i;
            let mut depth = 0;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'-') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'-' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            wipe(&mut out, start, i.min(b.len()));
        } else if b[i] == b'"' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(b.len());
            if strings {
                wipe(&mut out, start, i);
            }
        } else {
            i += 1;
        }
    }
    // Only ASCII bytes were replaced, by ASCII: still UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

fn ident_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(c, '_' | '\'' | '!' | '?' | '.' | '«' | '»')
        || ('\u{2080}'..='\u{209c}').contains(&c)
}

/// The identifiers in `text`, with their byte offsets.
fn idents(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        match (start, ident_char(c)) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                let w = text[s..i].trim_matches('.');
                if w.chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '«')
                {
                    out.push((s, w));
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

const KEYWORDS: &[&str] = &[
    "theorem",
    "lemma",
    "def",
    "abbrev",
    "instance",
    "structure",
    "inductive",
    "class",
    "axiom",
    "opaque",
    "example",
];
const MODIFIERS: &[&str] = &[
    "private",
    "protected",
    "noncomputable",
    "partial",
    "unsafe",
    "nonrec",
    "scoped",
    "local",
];

struct Decl {
    full: String,
    keyword: &'static str,
    rel: String,
    line: usize,
    /// The namespaces it was declared in, innermost last, and those open.
    namespaces: Vec<String>,
    opens: Vec<String>,
    simp: bool,
    /// Byte spans in the file: the whole declaration, and its statement
    /// (the type, before `:=`).
    span: (usize, usize),
    statement: (usize, usize),
    /// For an instance with no name: `instance : Add Point`.
    shown: String,
}

struct File<'a> {
    source: &'a Source,
    /// Comments and strings blanked.
    code: String,
    /// Comments blanked, strings kept.
    with_strings: String,
    decls: Vec<Decl>,
    /// `attribute [simp] a b` lines: the names, resolved later.
    simp_attributes: Vec<(String, Vec<String>, Vec<String>)>,
}

impl<'a> File<'a> {
    fn read(source: &'a Source) -> File<'a> {
        let code = blank(&source.text, true);
        let with_strings = blank(&source.text, false);
        let mut decls: Vec<Decl> = Vec::new();
        let mut simp_attributes = Vec::new();
        // The scopes open: (`namespace`, name) or (`section`, name).
        let mut scopes: Vec<(bool, String)> = Vec::new();
        let mut opens: Vec<String> = Vec::new();
        let mut attrs = String::new();
        let mut offset = 0;
        for raw in code.split_inclusive('\n') {
            let line_start = offset;
            offset += raw.len();
            let line = raw.trim_end();
            if line.trim().is_empty() || line.starts_with(char::is_whitespace) {
                continue;
            }
            let mut words = line.split_whitespace().peekable();
            let first = *words.peek().unwrap_or(&"");
            let namespaces = || {
                scopes
                    .iter()
                    .filter(|(ns, _)| *ns)
                    .map(|(_, n)| n.clone())
                    .collect::<Vec<_>>()
            };
            match first {
                "namespace" => {
                    if let Some(n) = line.split_whitespace().nth(1) {
                        // `namespace A.B` opens two.
                        for part in n.split('.') {
                            scopes.push((true, part.to_string()));
                        }
                    }
                    close_decl(&mut decls, line_start);
                    continue;
                }
                "section" => {
                    scopes.push((
                        false,
                        line.split_whitespace().nth(1).unwrap_or("").to_string(),
                    ));
                    close_decl(&mut decls, line_start);
                    continue;
                }
                "end" => {
                    let name = line.split_whitespace().nth(1).unwrap_or("");
                    let parts = name.split('.').filter(|p| !p.is_empty()).count().max(1);
                    for _ in 0..parts {
                        scopes.pop();
                    }
                    close_decl(&mut decls, line_start);
                    continue;
                }
                "open" => {
                    let full: Vec<String> = namespaces();
                    for n in line.split_whitespace().skip(1).take_while(|w| {
                        !w.starts_with('(') && *w != "in" && *w != "hiding" && *w != "renaming"
                    }) {
                        opens.push(n.to_string());
                        // An open inside a namespace may name a sibling.
                        if !full.is_empty() {
                            opens.push(format!("{}.{n}", full.join(".")));
                        }
                    }
                    close_decl(&mut decls, line_start);
                    continue;
                }
                "attribute" => {
                    // `attribute [simp] foo bar`
                    let with = &with_strings[line_start..line_start + line.len()];
                    if let (Some(open), Some(close)) = (with.find('['), with.find(']'))
                        && with[open + 1..close].split(',').any(|a| a.trim() == "simp")
                    {
                        let names = with[close + 1..]
                            .split_whitespace()
                            .map(str::to_string)
                            .collect();
                        simp_attributes.push((names_join(&namespaces()), opens.clone(), names));
                    }
                    close_decl(&mut decls, line_start);
                    continue;
                }
                _ => {}
            }
            // Attributes, then modifiers, then the keyword.
            let mut rest = line;
            let mut found_attrs = String::new();
            while let Some(r) = rest.trim_start().strip_prefix("@[") {
                let end = r.find(']').map_or(r.len(), |e| e + 1);
                found_attrs.push_str(
                    &with_strings[line_start + (line.len() - r.len())..][..end.min(r.len())],
                );
                rest = &r[end.min(r.len())..];
            }
            let mut words = rest.split_whitespace();
            let mut keyword = None;
            for w in words.by_ref() {
                if MODIFIERS.contains(&w) {
                    continue;
                }
                keyword = KEYWORDS.iter().find(|k| **k == w).copied();
                break;
            }
            let Some(keyword) = keyword else {
                if !found_attrs.is_empty() && rest.trim().is_empty() {
                    // An attribute on its own line, for the next declaration.
                    attrs.push_str(&found_attrs);
                } else {
                    // Another command at column 0 ends the declaration.
                    if !matches!(first, "|" | "where" | "with" | "deriving")
                        && !first.starts_with('.')
                    {
                        close_decl(&mut decls, line_start);
                    }
                    attrs.clear();
                }
                continue;
            };
            close_decl(&mut decls, line_start);
            attrs.push_str(&found_attrs);
            let after_kw = rest.find(keyword).map_or(0, |k| k + keyword.len());
            let head = &rest[after_kw..];
            let name = head.split_whitespace().next().filter(|n| {
                n.chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '«')
            });
            let name = name.map(|n| n.trim_end_matches(':').to_string());
            let ns = namespaces();
            let at = line_at(&source.text, line_start);
            let (full, shown) = match (keyword, name) {
                ("example", _) => (
                    format!("example@{}:{at}", source.rel),
                    format!("example (line {at})"),
                ),
                ("instance", None) => {
                    let ty = head
                        .trim()
                        .trim_start_matches(':')
                        .split(":=")
                        .next()
                        .unwrap_or("")
                        .split(" where")
                        .next()
                        .unwrap_or("")
                        .trim();
                    (
                        format!("instance@{}:{at}", source.rel),
                        format!("instance : {ty}"),
                    )
                }
                (_, Some(n)) => {
                    let n = n
                        .strip_prefix("_root_.")
                        .map(str::to_string)
                        .unwrap_or_else(|| names_join(&[names_join(&ns), n.clone()]));
                    (n.clone(), n)
                }
                (_, None) => continue,
            };
            let simp = attrs.trim_start_matches("@[").split([',', ']']).any(|a| {
                a.trim().trim_start_matches("@[").split_whitespace().next() == Some("simp")
            });
            attrs.clear();
            let start = line_start + (line.len() - rest.len()) + after_kw;
            decls.push(Decl {
                full,
                keyword,
                rel: source.rel.clone(),
                line: at,
                namespaces: ns,
                opens: opens.clone(),
                simp,
                span: (start, code.len()),
                statement: (start, code.len()),
                shown,
            });
        }
        // Each statement: to the first `:=` (or `where`, `|`) after it.
        for d in &mut decls {
            let body = &code[d.span.0..d.span.1];
            let end = [
                body.find(":="),
                body.find(" where"),
                body.find("\n|"),
                body.find("\n  |"),
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(body.len());
            d.statement = (d.span.0, d.span.0 + end);
        }
        File {
            source,
            code,
            with_strings,
            decls,
            simp_attributes,
        }
    }
}

fn names_join(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(".")
}

/// Ends the declaration still open at `offset`.
fn close_decl(decls: &mut [Decl], offset: usize) {
    if let Some(d) = decls.last_mut()
        && d.span.1 > offset
    {
        d.span.1 = offset;
    }
}

/// The declaration `name` means where `d` uses it: itself, under one of
/// `d`'s namespaces (innermost first), or under an open one.
fn resolve<'d>(
    name: &str,
    namespaces: &[String],
    opens: &[String],
    index: &HashMap<&str, usize>,
    decls: &[&'d Decl],
) -> Option<&'d Decl> {
    for k in (0..=namespaces.len()).rev() {
        let candidate = names_join(&[names_join(&namespaces[..k]), name.to_string()]);
        if let Some(i) = index.get(candidate.as_str()) {
            return Some(decls[*i]);
        }
    }
    opens
        .iter()
        .rev()
        .find_map(|o| index.get(format!("{o}.{name}").as_str()))
        .map(|i| decls[*i])
}

fn kind_of(keyword: &str) -> &'static str {
    match keyword {
        "theorem" | "lemma" => "theorem",
        "def" | "abbrev" | "opaque" => "def",
        "instance" => "instance",
        "axiom" => "axiom",
        "example" => "example",
        _ => "type",
    }
}

fn decl_node(map: &mut MapBuilder, d: &Decl) -> Option<usize> {
    let at = map.node(kind_of(d.keyword), &d.full, &d.shown);
    map.place(at, &d.rel, d.line);
    if d.simp {
        map.detail(at, "@[simp]");
    }
    at
}

fn theorems(files: &[File], decls: &[&Decl], index: &HashMap<&str, usize>) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "lean.theorems",
        "Theorem dependencies",
        "each theorem and the project's theorems, definitions and structures its statement and proof use, resolved through namespaces and opens",
    );
    for (n, f) in files.iter().enumerate() {
        for d in f.decls.iter() {
            let _ = n;
            if !matches!(d.keyword, "theorem" | "lemma" | "example") {
                continue;
            }
            let me = decl_node(&mut map, d);
            let mut seen = HashSet::new();
            for (_, word) in idents(&f.code[d.span.0..d.span.1]) {
                let Some(used) = resolve(word, &d.namespaces, &d.opens, index, decls) else {
                    continue;
                };
                if used.full == d.full || !seen.insert(used.full.clone()) {
                    continue;
                }
                let to = decl_node(&mut map, used);
                map.edge(me, to, "uses");
            }
        }
    }
    map.finish()
}

/// What a notation line declares: its symbol, and the text after `=>`.
fn notation_of(line: &str) -> Option<(String, String, String)> {
    let first = line.split_whitespace().next()?;
    let command = first.split(':').next()?;
    if !matches!(
        command,
        "notation" | "infix" | "infixl" | "infixr" | "prefix" | "postfix"
    ) {
        return None;
    }
    let (pattern, meaning) = line.split_once("=>")?;
    // The symbol: the pattern's string literals, joined.
    let mut symbol = String::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let close = after.find('"')?;
        symbol.push_str(after[..close].trim());
        rest = &after[close + 1..];
    }
    if symbol.is_empty() {
        return None;
    }
    Some((symbol, first.to_string(), meaning.trim().to_string()))
}

fn notation(files: &[File], decls: &[&Decl], index: &HashMap<&str, usize>) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "lean.notation",
        "Notation and simp",
        "each notation and the definition it stands for; each simp lemma and the notation or definition its left-hand side rewrites",
    );
    // Notations, by symbol: the node and the definition behind it.
    let mut symbols: Vec<(String, Option<usize>)> = Vec::new();
    for f in files {
        let mut offset = 0;
        for raw in f.with_strings.split_inclusive('\n') {
            let start = offset;
            offset += raw.len();
            let Some((symbol, command, meaning)) = notation_of(raw.trim()) else {
                continue;
            };
            let at = map.node("notation", &symbol, &symbol);
            map.place(at, &f.source.rel, line_at(&f.source.text, start));
            map.detail(at, command);
            // In scope where the line is: the declaration before it says.
            let (ns, opens) = f
                .decls
                .iter()
                .rev()
                .find(|d| d.span.0 <= start)
                .map(|d| (d.namespaces.clone(), d.opens.clone()))
                .unwrap_or_default();
            let target = idents(&blank(&meaning, true))
                .into_iter()
                .find_map(|(_, w)| resolve(w, &ns, &opens, index, decls));
            match target {
                Some(t) => {
                    let to = decl_node(&mut map, t);
                    map.edge(at, to, "means");
                }
                None => {
                    if let Some((_, w)) = idents(&meaning).first() {
                        let to = map.node("def", w, w);
                        map.edge(at, to, "means");
                    }
                }
            }
            symbols.push((symbol, at));
        }
    }
    // The simp lemmas: marked `@[simp]`, or named by `attribute [simp]`.
    let mut simp: Vec<&Decl> = decls.iter().copied().filter(|d| d.simp).collect();
    for f in files {
        for (ns, opens, names) in &f.simp_attributes {
            let ns: Vec<String> = ns
                .split('.')
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect();
            for n in names {
                if let Some(d) = resolve(n, &ns, opens, index, decls)
                    && !simp.iter().any(|s| s.full == d.full)
                {
                    simp.push(d);
                }
            }
        }
    }
    if !simp.is_empty() {
        let set = map.node("simp", "simp set", "simp set");
        map.detail(set, format!("{} lemmas", simp.len()));
        for d in simp {
            let lemma = decl_node(&mut map, d);
            map.edge(set, lemma, "simp");
            let f = &files[files
                .iter()
                .position(|f| f.source.rel == d.rel)
                .unwrap_or(0)];
            let Some(lhs) = lhs(&f.with_strings[d.statement.0..d.statement.1]) else {
                continue;
            };
            // What it rewrites: the definition its left side applies, else
            // the notation at its top, else any notation or definition in it.
            let blanked = blank(&lhs, true);
            let words = idents(&blanked);
            let known = |w: &str| {
                resolve(w, &d.namespaces, &d.opens, index, decls).filter(|t| t.full != d.full)
            };
            let head = words
                .first()
                .filter(|(at, _)| *at == 0)
                .and_then(|(_, w)| known(w));
            let top = top_level(&lhs);
            let rewrites = match head {
                Some(t) => decl_node(&mut map, t),
                None => symbols
                    .iter()
                    .find(|(s, _)| top.contains(s.as_str()))
                    .or_else(|| symbols.iter().find(|(s, _)| lhs.contains(s.as_str())))
                    .and_then(|(_, at)| *at)
                    .or_else(|| {
                        words
                            .iter()
                            .find_map(|(_, w)| known(w))
                            .and_then(|t| decl_node(&mut map, t))
                    }),
            };
            if rewrites.is_some() {
                map.edge(lemma, rewrites, "rewrites");
            }
        }
    }
    map.finish()
}

/// `text` less what is inside brackets.
fn top_level(text: &str) -> String {
    let mut depth = 0i32;
    text.chars()
        .filter(|c| {
            match c {
                '(' | '[' | '{' | '⟨' => depth += 1,
                ')' | ']' | '}' | '⟩' => {
                    depth -= 1;
                    return false;
                }
                _ => {}
            }
            depth == 0
        })
        .collect()
}

/// A statement's left-hand side: its type (after the binders' `:`) up to
/// the top-level `=` or `↔`.
fn lhs(statement: &str) -> Option<String> {
    let mut depth = 0i32;
    let mut colon = None;
    let chars: Vec<(usize, char)> = statement.char_indices().collect();
    for (k, &(i, c)) in chars.iter().enumerate() {
        match c {
            '(' | '[' | '{' | '⦃' | '⟨' => depth += 1,
            ')' | ']' | '}' | '⦄' | '⟩' => depth -= 1,
            ':' if depth == 0 && colon.is_none() && chars.get(k + 1).is_none_or(|n| n.1 != '=') => {
                colon = Some(i + 1)
            }
            _ => {}
        }
    }
    let ty = &statement[colon?..];
    let mut depth = 0i32;
    for (i, c) in ty.char_indices() {
        match c {
            '(' | '[' | '{' | '⟨' => depth += 1,
            ')' | ']' | '}' | '⟩' => depth -= 1,
            '↔' if depth == 0 => return Some(ty[..i].trim().to_string()),
            '=' if depth == 0 => {
                let prev = ty[..i].chars().last();
                let next = ty[i + 1..].chars().next();
                if !matches!(prev, Some(':' | '=' | '!' | '<' | '>'))
                    && !matches!(next, Some('=' | '>'))
                {
                    return Some(ty[..i].trim().to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(map: &ProjectMap) -> Vec<(String, String, String)> {
        let mut out: Vec<_> = map
            .edges
            .iter()
            .map(|e| {
                (
                    map.nodes[e.from].label.clone(),
                    map.nodes[e.to].label.clone(),
                    e.label.clone(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn e(a: &str, b: &str, l: &str) -> (String, String, String) {
        (a.into(), b.into(), l.into())
    }

    const PROJECT: &[(&str, &str)] = &[
        (
            "Shop/Basic.lean",
            "/-! The basics. -/\nnamespace Shop\n\nstructure Cart where\n  items : List Nat\n\n/-- Two carts appended. -/\ndef Cart.append (a b : Cart) : Cart := ⟨a.items ++ b.items⟩\n\ninfixl:65 \" +++ \" => Cart.append\n\ndef total (c : Cart) : Nat := c.items.foldl (· + ·) 0\n\n@[simp]\ntheorem total_append (a b : Cart) : total (a +++ b) = total a + total b := by\n  simp [total, Cart.append] -- uses total_nil? no\n\n@[simp] theorem append_empty (a : Cart) : a +++ ⟨[]⟩ = a := by\n  cases a; simp [Cart.append]\n\ntheorem total_nonneg (c : Cart) : 0 ≤ total c := Nat.zero_le _\n\nend Shop\n",
        ),
        (
            "Shop/More.lean",
            "import Shop.Basic\nopen Shop\n\nnotation:50 a \" ≈ \" b => Shop.Same a b\n\ndef Shop.Same (a b : Cart) : Prop := total a = total b\n\ntheorem same_refl (a : Cart) : a ≈ a := rfl\n\ntheorem twice (a : Cart) : total (a +++ a) = total a + total a := by\n  rw [total_append]\n\nlemma uses_root : True := trivial\n\nattribute [simp] same_refl\n\ninstance : Inhabited Cart := ⟨⟨[]⟩⟩\n\nexample : total ⟨[]⟩ = 0 := rfl\n",
        ),
    ];

    #[test]
    fn theorem_dependencies_through_namespaces_and_opens() {
        let maps = maps(&Corpus::of(PROJECT));
        let deps = maps.iter().find(|m| m.id == "lean.theorems").unwrap();
        let got = edges(deps);
        for want in [
            e("Shop.total_append", "Shop.total", "uses"),
            e("Shop.total_append", "Shop.Cart", "uses"),
            e("Shop.total_append", "Shop.Cart.append", "uses"),
            e("Shop.append_empty", "Shop.Cart.append", "uses"),
            e("Shop.total_nonneg", "Shop.total", "uses"),
            e("same_refl", "Shop.Cart", "uses"),
            e("twice", "Shop.total", "uses"),
            e("twice", "Shop.total_append", "uses"),
            e("example (line 19)", "Shop.total", "uses"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        // Comments and strings name nothing.
        assert!(
            !got.iter().any(|(_, b, _)| b == "Shop.total_nil"),
            "{got:?}"
        );
        let node = |l: &str| deps.nodes.iter().find(|n| n.label == l).unwrap();
        assert_eq!(
            (
                node("Shop.total_append").file.as_str(),
                node("Shop.total_append").line
            ),
            ("Shop/Basic.lean", 15)
        );
        assert_eq!(node("Shop.total_append").detail, "@[simp]");
        assert_eq!(node("Shop.total").line, 12);
        assert_eq!(node("uses_root").kind, "theorem");
    }

    #[test]
    fn notation_and_what_simp_rewrites() {
        let maps = maps(&Corpus::of(PROJECT));
        let n = maps.iter().find(|m| m.id == "lean.notation").unwrap();
        let got = edges(n);
        for want in [
            e("+++", "Shop.Cart.append", "means"),
            e("≈", "Shop.Same", "means"),
            e("simp set", "Shop.total_append", "simp"),
            e("simp set", "Shop.append_empty", "simp"),
            e("simp set", "same_refl", "simp"),
            e("Shop.total_append", "Shop.total", "rewrites"),
            e("Shop.append_empty", "+++", "rewrites"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert!(
            !got.iter().any(|(_, b, _)| b == "twice"),
            "not simp: {got:?}"
        );
        let node = |l: &str| n.nodes.iter().find(|x| x.label == l).unwrap();
        assert_eq!(
            (
                node("+++").file.as_str(),
                node("+++").line,
                node("+++").detail.as_str()
            ),
            ("Shop/Basic.lean", 10, "infixl:65")
        );
        assert_eq!(node("simp set").detail, "3 lemmas");
    }

    #[test]
    fn statements_left_hand_sides() {
        assert_eq!(
            lhs(" (a b : Cart) : total (a +++ b) = total a + total b ").as_deref(),
            Some("total (a +++ b)")
        );
        assert_eq!(
            lhs(" {p q : Prop} : p ∧ q ↔ q ∧ p ").as_deref(),
            Some("p ∧ q")
        );
        assert_eq!(lhs(" (n : Nat) : n ≤ n + 1 "), None);
        assert_eq!(lhs(" : (x == y) = true ").as_deref(), Some("(x == y)"));
    }

    #[test]
    fn blanking_keeps_offsets() {
        let text = "a -- b\n/- c /- d -/ e -/ f \"g\" h";
        let code = blank(text, true);
        assert_eq!(code.len(), text.len());
        assert_eq!(
            idents(&code)
                .into_iter()
                .map(|(_, w)| w)
                .collect::<Vec<_>>(),
            ["a", "f", "h"]
        );
        assert!(blank(text, false).contains("\"g\""));
    }
}
