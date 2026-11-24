//! Lean files, read one command at a time.
//!
//! Lean's syntax is extensible: Mathlib alone adds hundreds of commands and
//! notations (`assert_not_exists …`, `initialize_simps_projections …`,
//! `⁻¹`), which no fixed grammar knows. tree-sitter-lean's error recovery
//! does not stop at the end of the command it could not read: one unknown
//! command read as an expression swallowed every declaration after it, and a
//! Mathlib file came out with no outline at all. Lean itself ends a command
//! where the next one starts, at the start of a line; so does this module,
//! and each command is parsed on its own — an error costs its own command
//! and nothing else.

use std::ops::Range;
use std::path::{Path, PathBuf};

/// What a line that continues the command above it can start with, at its
/// start: a match alternative, a closing bracket, a `where` or `with` clause,
/// a proof's tail.
const CONTINUATIONS: &[&str] = &[
    "where",
    "with",
    "termination_by",
    "decreasing_by",
    "else",
    "then",
    "deriving",
    "at",
    "using",
    "from",
];

/// The byte ranges of `source`'s commands, in order and covering it: each
/// starts at the start of a line, where a command can (see [`starts_command`])
/// — outside a block comment, and not right below a line of attributes alone
/// (`@[simp]`), which belongs to the declaration under it.
///
/// Inside a namespace, a section or a `mutual` block written indented (as
/// Lean's own sources and Theorem Proving in Lean write them), each
/// declaration at the block's own indent starts a command too, with the doc
/// comment and attributes right above it: the block had no command start
/// but its `end`, and its declarations were read as one. Only a
/// declaration says what the indent is, and only outside brackets: a
/// body's line (`if …` under a `def`), a field's doc, or a quotation
/// (`` `(theorem …) ``) starts nothing.
pub(crate) fn commands(source: &str) -> Vec<Range<usize>> {
    let mut starts = vec![0];
    // Open `/- … -/` comments (they nest) at the start of the line.
    let mut depth = 0usize;
    // Whether the last line with anything on it held attributes alone.
    let mut attributes_above = false;
    // The blocks open — namespaces, sections, `mutual` — each with its own
    // indent, and the indent its commands are written at, once an indented
    // one says.
    let mut blocks: Vec<(usize, Option<usize>)> = Vec::new();
    // Brackets the command's lines so far leave open.
    let mut open = 0i32;
    // A doc comment and attributes, indented, right above the line, and at
    // what indent: the doc a command of its own, as at the top; the
    // attributes the start of the declaration's under them.
    let mut lead: Option<Lead> = None;
    // A string literal left open at the end of the line above: the next
    // line is its text, whatever its first column holds.
    let mut in_string = false;
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        let indent = text.len() - text.trim_start().len();
        let inner = text.trim_start();
        let free = depth == 0 && !in_string;
        if offset == 0 {
            enter(&mut blocks, text, 0);
        } else if free && !attributes_above && starts_command(text) {
            starts.push(offset);
            open = 0;
            lead = None;
            enter(&mut blocks, text, 0);
        } else if free && indent > 0 && open <= 0 && !inner.is_empty() {
            let leads = inner.starts_with("/--") || only_attributes(inner);
            let ends = matches!(inner.split_whitespace().next(), Some("end"))
                && blocks.last().is_some_and(|b| b.0 == indent);
            let declares_here =
                declares(inner) || matches!(inner.split_whitespace().next(), Some("section"));
            let fits = |b: &(usize, Option<usize>)| b.1.is_none_or(|at| at == indent);
            match blocks.last_mut() {
                Some(block) if leads && fits(block) => {
                    let above = lead.take().filter(|l| l.indent == indent);
                    lead = Some(match inner.starts_with("/--") {
                        true => Lead {
                            indent,
                            doc: Some(offset),
                            attributes: None,
                        },
                        false => Lead {
                            indent,
                            doc: above.and_then(|l| l.doc),
                            attributes: above.and_then(|l| l.attributes).or(Some(offset)),
                        },
                    });
                }
                Some(block) if ends || (declares_here && fits(block)) => {
                    if !ends {
                        block.1 = Some(indent);
                    }
                    let lead = lead.take().filter(|l| l.indent == indent);
                    let mut push = |start: usize| {
                        if starts.last().is_some_and(|&last| last < start) {
                            starts.push(start);
                        }
                    };
                    if let Some(doc) = lead.and_then(|l| l.doc) {
                        push(doc);
                    }
                    push(lead.and_then(|l| l.attributes).unwrap_or(offset));
                    open = 0;
                    enter(&mut blocks, inner, indent);
                }
                _ => lead = None,
            }
        }
        let opened = depth == 0;
        let brackets;
        (depth, in_string, brackets) = scan_line(text, depth, in_string);
        open += brackets;
        if !text.trim().is_empty() {
            attributes_above = opened && depth == 0 && only_attributes(text);
        }
        offset += line.len();
    }
    starts.push(source.len());
    starts
        .windows(2)
        .filter(|w| w[0] < w[1])
        .map(|w| w[0]..w[1])
        .collect()
}

/// A doc comment and attributes written, indented, above a declaration
/// ([`commands`]): where each starts.
#[derive(Clone, Copy)]
struct Lead {
    indent: usize,
    doc: Option<usize>,
    attributes: Option<usize>,
}

/// A command's first line `line`, at `indent`, opening a block
/// (`namespace`, `section`, `mutual`) or closing one (`end`), on the stack
/// of those open.
fn enter(blocks: &mut Vec<(usize, Option<usize>)>, line: &str, indent: usize) {
    match scoping(line) {
        Scoping::Namespace(_) | Scoping::Block => blocks.push((indent, None)),
        Scoping::End => {
            blocks.pop();
        }
        Scoping::Other => {}
    }
}

/// What a command does to the scopes of a Lean file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scoping {
    /// `namespace A.B`: names in it are `A.B.…`.
    Namespace(String),
    /// `section`, `noncomputable section`, `public section`, a `mutual`
    /// block: closed by an `end` of its own, and naming nothing.
    Block,
    /// `end`, `end A.B`: the innermost open one closed.
    End,
    /// Anything else.
    Other,
}

fn scoping(command: &str) -> Scoping {
    let mut rest = command.trim_start();
    while let Some(inside) = rest.strip_prefix("@[") {
        let Some(close) = closing_bracket(inside) else {
            return Scoping::Other;
        };
        rest = inside[close + 1..].trim_start();
    }
    let mut words = rest.split_whitespace();
    let mut word = words.next();
    while matches!(word, Some("noncomputable" | "public" | "private" | "meta")) {
        word = words.next();
    }
    match word {
        Some("namespace") => match words.next() {
            Some(name) => Scoping::Namespace(name.to_string()),
            None => Scoping::Other,
        },
        Some("section" | "mutual") => Scoping::Block,
        Some("end") => Scoping::End,
        _ => Scoping::Other,
    }
}

/// The scopes of a Lean file read through its `commands`: for each command,
/// the namespace it is in (`Clew.Extracted`, empty at the top); and for each
/// `namespace` command, by its index, the index of the command that ends it
/// — its `end`, or `None` where the file ends first.
pub(crate) fn scopes(
    source: &str,
    commands: &[Range<usize>],
) -> (Vec<String>, std::collections::HashMap<usize, Option<usize>>) {
    let mut open: Vec<(usize, Option<String>)> = Vec::new();
    let mut paths = Vec::with_capacity(commands.len());
    let mut ends = std::collections::HashMap::new();
    let path = |open: &[(usize, Option<String>)]| -> String {
        open.iter()
            .filter_map(|(_, n)| n.as_deref())
            .collect::<Vec<_>>()
            .join(".")
    };
    for (i, range) in commands.iter().enumerate() {
        paths.push(path(&open));
        match scoping(&source[range.clone()]) {
            Scoping::Namespace(name) => {
                ends.insert(i, None);
                open.push((i, Some(name)));
            }
            Scoping::Block => open.push((i, None)),
            Scoping::End => {
                if let Some((at, Some(_))) = open.pop() {
                    ends.insert(at, Some(i));
                }
            }
            Scoping::Other => {}
        }
    }
    (paths, ends)
}

/// For a Lean file written for the module system — its first command
/// `module` — the byte ranges of its public sections (`public section`,
/// `@[expose] public section`, to their `end`): there, a declaration is
/// public; elsewhere only one marked `public` is. `None` for a file of no
/// module, whose declarations are public unless `private`.
pub(crate) fn module_publics(source: &str) -> Option<Vec<Range<usize>>> {
    let commands = commands(source);
    let first = commands.iter().find_map(|r| {
        let code: String = source[r.clone()]
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        let code = code.trim();
        (!code.is_empty() && !code.starts_with("/-")).then(|| code.to_string())
    })?;
    if first.split_whitespace().next() != Some("module") {
        return None;
    }
    let mut open: Vec<(usize, bool)> = Vec::new();
    let mut publics = Vec::new();
    for range in &commands {
        let text = &source[range.clone()];
        match scoping(text) {
            Scoping::Namespace(_) => open.push((range.start, false)),
            Scoping::Block => {
                let head: Vec<&str> = text
                    .split_whitespace()
                    .take_while(|w| *w != "section")
                    .collect();
                let public =
                    text.split_whitespace().any(|w| w == "section") && head.contains(&"public");
                open.push((range.start, public));
            }
            Scoping::End => {
                if let Some((start, true)) = open.pop() {
                    publics.push(start..range.end);
                }
            }
            Scoping::Other => {}
        }
    }
    // A public section the file ends inside runs to its end.
    publics.extend(
        open.into_iter()
            .filter(|(_, public)| *public)
            .map(|(start, _)| start..source.len()),
    );
    Some(publics)
}

/// The blocks of a Lean file a reader folds — each `namespace`, `section`
/// and `mutual` block — as 0-based `(first, last)` lines: from its opening
/// line through the line before its `end`, which stays in view, as a
/// closing brace does. Their bodies are not indented, so the folds by
/// indent never had them.
pub fn blocks(source: &str) -> Vec<(usize, usize)> {
    let mut open: Vec<usize> = Vec::new();
    let mut out = Vec::new();
    let (mut line, mut counted_to) = (0, 0);
    for range in commands(source) {
        line += source[counted_to..range.start].matches('\n').count();
        counted_to = range.start;
        match scoping(&source[range]) {
            Scoping::Namespace(_) | Scoping::Block => open.push(line),
            Scoping::End => {
                if let Some(first) = open.pop()
                    && line > first + 1
                {
                    out.push((first, line - 1));
                }
            }
            Scoping::Other => {}
        }
    }
    out.sort_unstable();
    out
}

/// Whether a line (its newline cut) can start a command: it starts at its
/// first column, and not with what continues one (see [`CONTINUATIONS`]).
fn starts_command(line: &str) -> bool {
    let Some(first) = line.chars().next() else {
        return false;
    };
    if first.is_whitespace() || matches!(first, '|' | ')' | ']' | '}' | ',' | '⟩' | '·' | '.') {
        return false;
    }
    let word = line
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | '[' | '{' | ':'))
        .next()
        .unwrap_or_default();
    !CONTINUATIONS.contains(&word)
}

/// How many `/- … -/` comments are open after `line`, `depth` being open
/// before it, and whether a string literal is still open at its end (a
/// string runs on over lines), the line starting inside one (`in_string`)
/// or not. Block comments nest; a `--` outside one ends the line's code; a
/// `/-` in a string literal opens nothing.
///
/// And how many more brackets — `(`, `[`, `{` — the line opens than it
/// closes, outside comments and literals.
fn scan_line(line: &str, mut depth: usize, mut in_string: bool) -> (usize, bool, i32) {
    let b = line.as_bytes();
    let mut brackets = 0;
    let mut i = 0;
    while i < b.len() {
        let pair = |a: u8, c: u8| b[i] == a && b.get(i + 1) == Some(&c);
        if depth > 0 {
            if pair(b'/', b'-') {
                depth += 1;
                i += 2;
            } else if pair(b'-', b'/') {
                depth -= 1;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_string {
            match b[i] {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        if pair(b'-', b'-') {
            break;
        }
        // A character literal: `'"'`, `'\"'` open no string.
        if let Some(end) = char_literal_end(b, i) {
            i = end;
            continue;
        }
        if pair(b'/', b'-') {
            depth += 1;
            i += 2;
            continue;
        }
        match b[i] {
            b'"' => in_string = true,
            b'(' | b'[' | b'{' => brackets += 1,
            b')' | b']' | b'}' => brackets -= 1,
            _ => {}
        }
        i += 1;
    }
    (depth, in_string, brackets)
}

/// `command` as the grammar can read its declaration: the arguments of its
/// attributes (`@[to_additive (attr := simp)]`, `@[inherit_doc A.b]`) and an
/// instance's priority (`instance (priority := 100) foo`) blanked, as the
/// grammar takes neither and reads a declaration carrying one as an
/// expression. Blanked to spaces of the same byte length, so every position
/// in it is the command's own; nothing else is changed, and the names, the
/// signatures and the docs read from it are the command's.
pub(crate) fn readable(command: &str) -> std::borrow::Cow<'_, str> {
    let mut blank: Vec<Range<usize>> = Vec::new();
    let mut rest = command.trim_start();
    let mut at = command.len() - rest.len();
    // The attributes and modifiers the declaration starts with.
    loop {
        if let Some(inside) = rest.strip_prefix("@[") {
            let Some(close) = closing_bracket(inside) else {
                break;
            };
            let start = at + 2;
            let mut entry_start = start;
            for (i, c) in inside[..close].char_indices().chain([(close, ',')]) {
                if c != ',' || bracket_depth(&inside[..i]) != 0 {
                    continue;
                }
                let entry = &command[entry_start..start + i];
                let name_end = entry
                    .trim_start()
                    .find([' ', '\t', '\n', '('])
                    .map_or(entry.len(), |n| entry.len() - entry.trim_start().len() + n);
                blank.push(entry_start + name_end..start + i);
                entry_start = start + i + 1;
            }
            let after = &inside[close + 1..];
            let skipped = after.len() - after.trim_start().len();
            at = start + close + 1 + skipped;
            rest = &after[skipped..];
            continue;
        }
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..word_end];
        if word == "instance" {
            let after = &rest[word_end..];
            let skipped = after.len() - after.trim_start().len();
            let open = at + word_end + skipped;
            if command[open..].starts_with("(priority")
                && let Some(close) = closing_paren(&command[open + 1..])
            {
                blank.push(open..open + 1 + close + 1);
            }
            break;
        }
        if !LEAN_MODIFIERS.contains(&word) || word.is_empty() {
            break;
        }
        let after = &rest[word_end..];
        let skipped = after.len() - after.trim_start().len();
        at += word_end + skipped;
        rest = &after[skipped..];
    }
    blank.retain(|r| r.start < r.end);
    if blank.is_empty() {
        return std::borrow::Cow::Borrowed(command);
    }
    let mut out = String::with_capacity(command.len());
    let mut from = 0;
    for r in blank {
        out.push_str(&command[from..r.start]);
        for c in command[r.clone()].chars() {
            match c {
                '\n' => out.push('\n'),
                c => out.extend(std::iter::repeat_n(' ', c.len_utf8())),
            }
        }
        from = r.end;
    }
    out.push_str(&command[from..]);
    std::borrow::Cow::Owned(out)
}

/// The keywords a declaration (or a namespace) starts with, past its
/// attributes and modifiers.
const DECLARATIONS: &[&str] = &[
    "def",
    "theorem",
    "lemma",
    "abbrev",
    "instance",
    "example",
    "structure",
    "class",
    "inductive",
    "axiom",
    "opaque",
    "constant",
    "namespace",
    "mutual",
];

/// Whether a line (trimmed) starts a declaration: its keyword, past any
/// attributes and modifiers.
pub(crate) fn declares(line: &str) -> bool {
    let mut rest = line;
    while let Some(inside) = rest.strip_prefix("@[") {
        let Some(close) = closing_bracket(inside) else {
            return false;
        };
        rest = inside[close + 1..].trim_start();
        // A comment after the attributes: the declaration is below it.
        if let Some(comment) = rest.strip_prefix("--") {
            rest = comment
                .split_once('\n')
                .map_or("", |(_, below)| below)
                .trim_start();
        }
    }
    rest.split_whitespace()
        .find(|w| !LEAN_MODIFIERS.contains(w))
        .is_some_and(|w| DECLARATIONS.contains(&w))
}

/// What a declaration's first line says, read from its text: for a command
/// the grammar cannot read, what the outline lists it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Head {
    /// The outline's kind for its keyword.
    pub kind: &'static str,
    /// Where its name is in the command.
    pub name: Range<usize>,
    /// Where its keyword is in the command.
    pub keyword: usize,
    /// Where its statement ends: its `:=`, `where` or first match
    /// alternative, outside brackets; the command's end without one.
    pub signature_end: usize,
}

/// The declaration `command` starts with — past its attributes and
/// modifiers, its keyword and its name — or `None` for another command.
/// An `example` has no name: it is listed as itself. An instance without
/// one — Lean makes one up (`instInhabitedNat`) — is listed by the class
/// it is an instance of, its statement's first line (`Inhabited Nat`).
pub(crate) fn head(command: &str) -> Option<Head> {
    let mut at = command.len() - command.trim_start().len();
    loop {
        let rest = &command[at..];
        if let Some(inside) = rest.strip_prefix("@[") {
            at += 2 + closing_bracket(inside)? + 1;
            at += command[at..].len() - command[at..].trim_start().len();
            continue;
        }
        // A comment after the attributes (`@[simp] -- why`): read past.
        if at > 0 && rest.starts_with("--") {
            at += rest.find('\n').unwrap_or(rest.len());
            at += command[at..].len() - command[at..].trim_start().len();
            continue;
        }
        let word = rest.split(char::is_whitespace).next().unwrap_or_default();
        if !LEAN_MODIFIERS.contains(&word) || word.is_empty() {
            break;
        }
        at += word.len();
        at += command[at..].len() - command[at..].trim_start().len();
    }
    let keyword = at;
    let rest = &command[at..];
    let word = rest.split(char::is_whitespace).next().unwrap_or_default();
    let mut kind = match word {
        "def" | "abbrev" | "opaque" => "function",
        "theorem" | "lemma" => "theorem",
        "instance" => "instance",
        "structure" => "struct",
        "class" => "class",
        "inductive" => "inductive",
        "axiom" => "axiom",
        "constant" => "constant",
        "example" => "example",
        // Notation, macros and syntax: named by what they write.
        "notation" | "infix" | "infixl" | "infixr" | "prefix" | "postfix" | "macro" | "syntax"
        | "elab" | "declare_syntax_cat" => return syntax_head(command, keyword, word),
        w if w.starts_with("notation")
            || w.starts_with("infix")
            || w.starts_with("prefix:")
            || w.starts_with("postfix:")
            || w.starts_with("macro:")
            || w.starts_with("syntax:")
            || w.starts_with("elab:") =>
        {
            return syntax_head(command, keyword, w);
        }
        _ => return None,
    };
    if word == "example" {
        let name = keyword..keyword + word.len();
        return Some(Head {
            kind,
            signature_end: statement_end(command, name.end),
            name,
            keyword,
        });
    }
    at += word.len();
    at += command[at..].len() - command[at..].trim_start().len();
    if word == "class" && command[at..].starts_with("inductive") {
        at += "inductive".len();
        at += command[at..].len() - command[at..].trim_start().len();
        kind = "class";
    }
    if word == "instance" && command[at..].starts_with("(priority") {
        at += 1 + closing_paren(&command[at + 1..])? + 1;
        at += command[at..].len() - command[at..].trim_start().len();
    }
    let name_len = name_length(&command[at..]);
    if name_len == 0 {
        return (word == "instance").then(|| unnamed_instance(command, at, keyword))?;
    }
    let name = at..at + name_len;
    Some(Head {
        kind,
        signature_end: statement_end(command, name.end),
        name,
        keyword,
    })
}

/// The head of a notation, macro or syntax declaration, keyword `word` at
/// `keyword`: named by its `(name := …)` where it gives one, else by what it
/// writes — its first string literal (`"+++"`); a syntax category by its
/// name (`declare_syntax_cat tactic_seq`).
fn syntax_head(command: &str, keyword: usize, word: &str) -> Option<Head> {
    let after = keyword + word.len();
    let end = command.len();
    let rest = &command[after..];
    let name = if word == "declare_syntax_cat" {
        let skipped = rest.len() - rest.trim_start().len();
        let len = name_length(&rest[skipped..]);
        (len > 0).then(|| after + skipped..after + skipped + len)?
    } else if let Some(at) = rest.find("(name :=") {
        let from = after + at + "(name :=".len();
        let skipped = command[from..].len() - command[from..].trim_start().len();
        let named = &command[from + skipped..];
        let len = name_length(named).min(named.find(')').unwrap_or(named.len()));
        (len > 0).then(|| from + skipped..from + skipped + len)?
    } else {
        let open = rest.find('"')?;
        let close = rest[open + 1..].find('"')?;
        let inner = &rest[open + 1..open + 1 + close];
        let lead = inner.len() - inner.trim_start().len();
        let from = after + open + 1 + lead;
        (!inner.trim().is_empty()).then(|| from..from + inner.trim().len())?
    };
    // Its statement: up to what it stands for (`=>`), on its first line.
    let line_end = command[keyword..].find('\n').map_or(end, |n| keyword + n);
    let signature_end = command[keyword..line_end]
        .find("=>")
        .map_or(line_end, |n| keyword + n);
    Some(Head {
        kind: "macro",
        name,
        keyword,
        signature_end,
    })
}

/// The head of an instance with no name, whose binders or `:` start at
/// `at`: named by its class, the statement after its `:` up to `:=` or
/// `where`, on that first line.
fn unnamed_instance(command: &str, at: usize, keyword: usize) -> Option<Head> {
    let end = statement_end(command, at);
    let mut depth = 0i32;
    let colon = command[at..end].char_indices().find_map(|(i, c)| {
        match c {
            '(' | '[' | '{' | '⦃' | '⟨' => depth += 1,
            ')' | ']' | '}' | '⦄' | '⟩' => depth -= 1,
            ':' if depth <= 0 => return Some(at + i),
            _ => {}
        }
        None
    })?;
    let rest = &command[colon + 1..end];
    let start = colon + 1 + (rest.len() - rest.trim_start().len());
    let line_end = command[start..end].find('\n').map_or(end, |i| start + i);
    let stop = start + command[start..line_end].trim_end().len();
    (stop > start).then_some(Head {
        kind: "instance",
        name: start..stop,
        keyword,
        signature_end: end,
    })
}

/// The length of the Lean name `text` starts with (`Group.toMonoid`,
/// `«sum ≤»`, `foo'`), or 0.
/// How long the name `text` starts with is, in bytes (`Foo.bar`,
/// `«a b»`): none where it starts with no name.
pub(crate) fn name_len(text: &str) -> usize {
    name_length(text)
}

fn name_length(text: &str) -> usize {
    let mut len = 0;
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '«' => quoted = true,
            '»' => quoted = false,
            _ if quoted => {}
            c if c.is_whitespace() || matches!(c, ':' | '(' | '[' | '{' | '⦃' | ',' | '⟨') => {
                break;
            }
            _ => {}
        }
        len += c.len_utf8();
    }
    // A name's universe parameters (`def div.inductionOn.{u}`) are no part
    // of it: read with it, the name ended in a dot.
    if text[..len].ends_with('.') && text[len..].starts_with('{') {
        len -= 1;
    }
    len
}

/// Whether `c` can start a Lean name: a letter (Greek and letter-like
/// symbols, `ℕ`, too), `_`, or the `«` of a quoted one.
pub(crate) fn starts_name(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '«' || ('\u{2100}'..='\u{214F}').contains(&c)
}

/// Where the statement after a declaration's name (at `from`) ends.
fn statement_end(command: &str, from: usize) -> usize {
    let mut depth = 0i32;
    let mut line_start = false;
    let text = &command[from..];
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' | '{' | '⦃' | '⟨' => depth += 1,
            ')' | ']' | '}' | '⦄' | '⟩' => depth -= 1,
            ':' if depth <= 0 && text[i..].starts_with(":=") => return from + i,
            '|' if depth <= 0 && line_start => return from + i,
            'w' if depth <= 0
                && text[i..].starts_with("where")
                && text[..i].ends_with(char::is_whitespace)
                && !text[i + 5..].starts_with(|c: char| c.is_alphanumeric() || c == '_') =>
            {
                return from + i;
            }
            _ => {}
        }
        if c == '\n' {
            line_start = true;
        } else if !c.is_whitespace() {
            line_start = false;
        }
    }
    command.trim_end().len().max(from)
}

/// The keywords that may come before a declaration's own.
const LEAN_MODIFIERS: &[&str] = &[
    "noncomputable",
    "partial",
    "protected",
    "private",
    "public",
    "meta",
    "unsafe",
    "scoped",
    "local",
    "nonrec",
];

/// Where the character literal at byte `i` of `b` ends (`'x'`, `'"'`,
/// `'\"'`, `'\n'`, `'∀'`), when one starts there: a quote no name runs
/// into (`h'` is a name), closed after one character or one escape.
/// `'\"'` read as a quote and a string, the string ran on to the end of
/// the file, and nothing after it was read.
pub(crate) fn char_literal_end(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'\'') {
        return None;
    }
    if i > 0
        && (b[i - 1].is_ascii_alphanumeric() || matches!(b[i - 1], b'_' | b'\'' | b'.' | 0x80..))
    {
        return None;
    }
    let first = *b.get(i + 1)?;
    if first == b'\\' {
        let close = b.get(i + 3..)?.iter().take(12).position(|&c| c == b'\'')?;
        return Some(i + 3 + close + 1);
    }
    let width = match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    };
    (b.get(i + 1 + width) == Some(&b'\'') && first != b'\'').then_some(i + 2 + width)
}

/// How many brackets are open at the end of `text`, string and character
/// literals aside.
fn bracket_depth(text: &str) -> i32 {
    let b = text.as_bytes();
    let mut depth = 0;
    let mut in_string = false;
    let mut i = 0;
    while i < b.len() {
        if in_string {
            match b[i] {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        if let Some(end) = char_literal_end(b, i) {
            i = end;
            continue;
        }
        match b[i] {
            b'"' => in_string = true,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    depth
}

/// The byte index of the `)` closing the parenthesis `text` follows.
fn closing_paren(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(i),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Whether a line holds attributes alone (`@[simp]`, `@[to_additive (attr :=
/// simp) "doc"] @[inline]`), a line comment aside.
fn only_attributes(line: &str) -> bool {
    let mut rest = line.trim();
    let mut any = false;
    while let Some(after) = rest.strip_prefix("@[") {
        let Some(close) = closing_bracket(after) else {
            return false;
        };
        rest = after[close + 1..].trim_start();
        any = true;
    }
    any && (rest.is_empty() || rest.starts_with("--"))
}

/// The byte index of the `]` closing the bracket `text` follows, brackets in
/// string and character literals aside.
fn closing_bracket(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut i = 0;
    while i < b.len() {
        if in_string {
            match b[i] {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        if let Some(end) = char_literal_end(b, i) {
            i = end;
            continue;
        }
        match b[i] {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' if depth == 0 => return Some(i),
            b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    None
}

/// The Lake project the Lean file `file` belongs to: the nearest folder
/// above it, up to `root`, holding a lakefile (`lakefile.lean` or
/// `lakefile.toml`) — where Lean's server must run for the file's imports
/// of its own project to resolve, in a repository whose Lean is in a
/// folder of its own. `root` for a file in no Lake project under it.
///
/// A dependency's file — Lake's checkout of it, in `.lake/packages/` (or
/// `lake-packages/` before Lake 5), where go to definition takes the
/// reader — is the project's that holds the checkout: its server has the
/// dependency on its path. Read as the dependency's own project, opening
/// one of Mathlib's files restarted Lean in Mathlib's checkout. `None` for
/// a file outside `root` (the toolchain's own sources): no project of the
/// opened folder's, and whatever server runs serves it.
pub fn lake_root(root: &Path, file: &Path) -> Option<PathBuf> {
    let rel = file.strip_prefix(root).ok()?;
    let parts: Vec<_> = rel.components().collect();
    let from = match parts
        .iter()
        .position(|c| matches!(c.as_os_str().to_str(), Some(".lake" | "lake-packages")))
    {
        Some(at) => parts[..at]
            .iter()
            .fold(root.to_path_buf(), |p, c| p.join(c)),
        None => file.parent()?.to_path_buf(),
    };
    let lakefile = |dir: &Path| {
        ["lakefile.lean", "lakefile.toml"]
            .iter()
            .any(|name| dir.join(name).is_file())
    };
    Some(
        from.ancestors()
            .take_while(|dir| dir.starts_with(root))
            .find(|dir| lakefile(dir))
            .map_or_else(|| root.to_path_buf(), Path::to_path_buf),
    )
}

/// Whether a Lean file has a program to run: a `def main` of its own, at
/// its top level and in no namespace (`partial def main`, `unsafe def
/// main` too), as `lean --run` needs one.
pub fn has_main(source: &str) -> bool {
    main_lines(source).is_some()
}

/// Where a Lean file's program starts, as 1-based lines: its `def main`'s
/// name's, and the first and last of its command. Only the root `main`
/// runs (`lean --run`): one in a namespace is `Foo.main`, and one in a
/// comment is none — each was offered to run, and Lean said "no main".
pub fn main_lines(source: &str) -> Option<(usize, (usize, usize))> {
    let commands = commands(source);
    let (paths, _) = scopes(source, &commands);
    let line_of = |at: usize| source[..at].matches('\n').count() + 1;
    commands.iter().zip(&paths).find_map(|(range, path)| {
        if !path.is_empty() {
            return None;
        }
        let command = &source[range.clone()];
        let head = head(command)?;
        (head.kind == "function" && &command[head.name.clone()] == "main").then(|| {
            let last = range.start + command.trim_end().len().max(1) - 1;
            (
                line_of(range.start + head.name.start),
                (line_of(range.start + head.keyword), line_of(last)),
            )
        })
    })
}

/// How a run of a Lean file's `main` ended.
pub use crate::spawn::RunEnd;

/// The modules a Lean file's header imports (`import A.B`, `public import
/// C`, `import all D`, several to a line), in order.
pub(crate) fn header_imports(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for range in commands(source) {
        let command = &source[range];
        let code: String = command
            .lines()
            .map(|l| l.split("--").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join(" ");
        let mut words = code.split_whitespace().peekable();
        while words
            .next_if(|w| matches!(*w, "public" | "private" | "meta"))
            .is_some()
        {}
        match words.next() {
            Some("import") => out.extend(
                words
                    .filter(|w| *w != "all")
                    .take_while(|w| name_len(w) == w.len())
                    .map(str::to_string),
            ),
            Some("module" | "prelude") | None => {}
            // A comment, or the file's first declaration: the header ends.
            Some(w) if w.starts_with("/-") => {}
            Some(_) => break,
        }
    }
    out
}

/// Whether module `module` is the toolchain's own (Lean's core: `Init`,
/// `Std`, `Lean`, `Lake`), built with it, and not the project's to build.
fn core_module(module: &str) -> bool {
    matches!(
        module.split('.').next(),
        Some("Init" | "Std" | "Lean" | "Lake")
    )
}

/// Run the `main` of the Lean file `file` as Lake runs one: what it
/// imports built first (`lake build +A.B`), then `lake env lean --run
/// <file>` in its Lake project `project`, with the project's dependencies
/// on Lean's path. Each line either writes is handed to `said` as it comes,
/// with whether it was written to stderr. Blocking; stopped, with
/// everything it started, once `cancel` is set.
///
/// `lake env` builds nothing: a `main` importing a module of its own
/// project ran into "unknown module prefix" until the project was built by
/// hand.
pub fn run_main(
    lake: &Path,
    project: &Path,
    file: &Path,
    mut said: impl FnMut(&str, bool),
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<RunEnd, String> {
    let source = crate::statefile::read_capped(file, 8 << 20).unwrap_or_default();
    let modules: Vec<String> = header_imports(&source)
        .into_iter()
        .filter(|m| !core_module(m))
        .collect();
    if !modules.is_empty() {
        let mut build = crate::spawn::command(lake);
        build
            .arg("build")
            .args(modules.iter().map(|m| format!("+{m}")))
            .current_dir(project)
            .env("PATH", crate::linerpc::child_path());
        match crate::spawn::run_streaming(build, &mut said, cancel)? {
            RunEnd::Exited(0) => {}
            ended => return Ok(ended),
        }
    }
    let mut command = crate::spawn::command(lake);
    command
        .args(["env", "lean", "--run"])
        .arg(file)
        .current_dir(project)
        .env("PATH", crate::linerpc::child_path());
    // Lean runs under Lake: stopping one stops both.
    crate::spawn::run_streaming(command, said, cancel)
}

/// Where the build of the Mathlib a Lake project at `root` depends on — or
/// is — belongs, when it is not there: `None` for a project without
/// Mathlib, and for one whose build is in place. Without it, Lean builds
/// all of Mathlib itself the first time a file importing it is checked,
/// which takes hours; `lake exe cache get` downloads it in minutes.
pub fn mathlib_build_missing(root: &Path) -> Option<PathBuf> {
    mathlib_build(root)
        .filter(|(_, built)| !built)
        .map(|(build, _)| build)
}

/// Mathlib's build for the Lake project at `root`: where it belongs, and
/// whether it is in place; `None` for a project without Mathlib.
pub fn mathlib_build(root: &Path) -> Option<(PathBuf, bool)> {
    use serde_json::Value;
    // Capped, and never blocked on: a repository's manifest that is a FIFO,
    // or a link to `/dev/zero`, hung or filled the app reading it.
    let manifest = crate::statefile::read_capped(&root.join("lake-manifest.json"), 4 << 20)?;
    let manifest: Value = serde_json::from_str(&manifest).ok()?;
    let named = |v: &Value| v.get("name").and_then(Value::as_str) == Some("mathlib");
    let mathlib = if named(&manifest) {
        root.to_path_buf()
    } else {
        let packages = manifest
            .get("packagesDir")
            .and_then(Value::as_str)
            .unwrap_or(".lake/packages");
        manifest
            .get("packages")?
            .as_array()?
            .iter()
            .any(named)
            .then(|| root.join(packages).join("mathlib"))?
    };
    let build = mathlib.join(".lake").join("build");
    // `lib/lean/` since Lake 5; `lib/` before.
    let built = ["lib/lean/Mathlib.olean", "lib/Mathlib.olean"]
        .iter()
        .any(|olean| build.join(olean).is_file());
    Some((build, built))
}

#[cfg(test)]
mod tests {

    /// A namespace written indented has its declarations read one by one —
    /// their docs and attributes with them, its `end` its own — while a
    /// structure's field docs, a body's lines and a quotation's
    /// declarations start nothing.
    #[test]
    fn an_indented_namespace_holds_its_declarations() {
        let src = "namespace A\n  /-- One. -/\n  def one := 1\n\n  @[simp]\n  theorem t : one = 1 := rfl\n\n  namespace B\n    def two := 2\n  end B\n\n  structure S where\n    /-- A field. -/\n    x : Nat\nend A\n\nnamespace C\ndef f (n : Nat) : Nat :=\n  if n = 0 then 0 else n\nmacro \"m\" : command => `(\n  theorem q : True := trivial)\nend C\n";
        let lines: Vec<usize> = commands(src)
            .iter()
            .map(|r| src[..r.start].matches('\n').count())
            .collect();
        assert_eq!(lines, [0, 1, 2, 4, 7, 8, 9, 11, 14, 16, 17, 19, 21]);
        let (paths, _) = scopes(src, &commands(src));
        assert_eq!(paths[5], "A.B", "{paths:?}");
        assert_eq!(paths[7], "A");
    }

    /// A character literal holds a quote without opening a string: an
    /// escaped one (`'\"'`) read as a string's start, the rest of the file
    /// was one string, and none of its declarations were read.
    #[test]
    fn a_char_literal_opens_no_string() {
        let src = "def q (c : Char) : Bool := c == '\\\"'\n\ndef r := 'x'\n\ndef s := ['\\'', '∀', '\"']\n\ndef t (h' : Nat) := h'\n";
        let lines: Vec<usize> = commands(src)
            .iter()
            .map(|r| src[..r.start].matches('\n').count())
            .collect();
        assert_eq!(lines, [0, 2, 4, 6]);
        let b = src.as_bytes();
        let at = |needle: &str| src.find(needle).unwrap();
        assert_eq!(char_literal_end(b, at("'\\\"'")), Some(at("'\\\"'") + 4));
        assert_eq!(char_literal_end(b, at("'∀'")), Some(at("'∀'") + 5));
        assert_eq!(char_literal_end(b, at("' : Nat")), None, "a name's prime");
        assert_eq!(bracket_depth("f ['(', ')'] (x"), 1);
    }

    /// In a `mutual` block, a declaration starts a command at the block's
    /// own indent, whether at column 0 or indented; the lines of a body
    /// start none, at whatever indent.
    #[test]
    fn a_mutual_blocks_bodies_start_no_commands() {
        let lines = |src: &str| -> Vec<usize> {
            commands(src)
                .iter()
                .map(|r| src[..r.start].matches('\n').count())
                .collect()
        };
        let flush = "mutual\ndef f (n : Nat) : Nat :=\n  if n = 0 then 0 else g (n - 1)\ndef g (n : Nat) : Nat :=\n  if n = 0 then 1 else f (n - 1)\nend\n";
        assert_eq!(lines(flush), [0, 1, 3, 5]);
        let indented = "mutual\n  def f (n : Nat) : Nat :=\n    if n = 0 then 0 else g (n - 1)\n  /-- The other. -/\n  def g (n : Nat) : Nat :=\n    f n\nend\n";
        // The doc comment a command of its own, as at the top.
        assert_eq!(lines(indented), [0, 1, 3, 4, 6]);
    }

    /// A namespace, a section and a `mutual` block each fold from their
    /// opening line to the line before their `end`; one too short to hide
    /// a line does not.
    #[test]
    fn lean_blocks_fold_to_their_end() {
        let src = "namespace A\n\ndef f := 1\n\nsection\ndef g := 2\nend\n\nmutual\n  def h : Nat := 1\nend\nend A\nsection\nend\n";
        assert_eq!(blocks(src), [(0, 10), (4, 5), (8, 9)]);
    }

    use super::*;

    /// A file runs when it has a `main` of its own, at its top level.
    #[test]
    fn a_file_with_a_main_has_something_to_run() {
        assert!(has_main("def main : IO Unit := IO.println \"hi\"\n"));
        assert!(has_main(
            "partial def main (args : List String) : IO UInt32 := pure 0\n"
        ));
        assert!(has_main("def main: IO Unit := pure ()\n"));
        assert!(!has_main("theorem t : 1 = 1 := rfl\n"));
        assert!(!has_main("def mainLoop : IO Unit := pure ()\n"));
        assert!(!has_main(
            "namespace X\n  def main : IO Unit := pure ()\nend X\n"
        ));
        assert!(!has_main("-- def main\n"));
        // In a namespace, even written at column 0, it is `X.main`; in a
        // block comment, none; in a section, the root's.
        assert!(!has_main(
            "namespace X\ndef main : IO Unit := pure ()\nend X\n"
        ));
        assert!(!has_main("/-\ndef main : IO Unit := pure ()\n-/\n"));
        assert!(has_main(
            "section\n/-- Greets. -/\n@[inline] def main : IO Unit := pure ()\nend\n"
        ));
        // Where its ▶ goes: its name's line, the command its span.
        assert_eq!(
            main_lines("import Lean\n\ndef main : IO Unit := do\n  IO.println 1\n"),
            Some((3, (3, 4)))
        );
    }

    /// Every Lean file under `CLEW_TEST_LEAN_SRC` (a toolchain's own
    /// sources: Lean, Lake, Std) read as Clew reads one — its commands,
    /// scopes, folds, outline, colours and weak spots — without a panic, in
    /// time; what reads oddly is listed: a declaration the outline lost, a
    /// name no Lean name is.
    #[test]
    #[ignore = "reads a Lean source tree (CLEW_TEST_LEAN_SRC=path); run explicitly"]
    fn live_a_lean_source_tree_reads_whole() {
        let root = PathBuf::from(
            std::env::var("CLEW_TEST_LEAN_SRC").expect("CLEW_TEST_LEAN_SRC names a folder"),
        );
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "lean") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(&root, &mut files);
        files.sort();
        let keywords = [
            "def",
            "theorem",
            "lemma",
            "abbrev",
            "structure",
            "class",
            "inductive",
        ];
        let mut slow = Vec::new();
        let mut lost = Vec::new();
        let mut odd = Vec::new();
        let (mut declared, mut found) = (0usize, 0usize);
        for file in &files {
            let source = std::fs::read_to_string(file).unwrap();
            let started = std::time::Instant::now();
            let commands = commands(&source);
            let _ = scopes(&source, &commands);
            let _ = blocks(&source);
            let _ = header_imports(&source);
            let located = crate::outline::extract_located(&source, "lean");
            let _ = crate::highlight::highlight_lines(&source, Some("lean"));
            let rel = file
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let _ = crate::weakspots::scan_file(&rel, &source, "lean");
            let took = started.elapsed();
            if took > std::time::Duration::from_secs(2) {
                slow.push((took, rel.clone()));
            }
            // Declarations at the start of a line, by their keyword past
            // modifiers and attributes, against the outline's.
            let named: std::collections::HashSet<usize> =
                located.iter().map(|l| l.symbol.line).collect();
            let mut in_comment = 0usize;
            for (i, line) in source.lines().enumerate() {
                let (depth, _, _) = scan_line(line, in_comment, false);
                let was = in_comment;
                in_comment = depth;
                if was > 0 {
                    continue;
                }
                let t = line.trim_start();
                // Indented, or written in a macro's quotation (`$typeName`):
                // no declaration of the file's own.
                if line.len() - t.len() > 2 || t.contains('$') {
                    continue;
                }
                let Some(h) = head(t) else { continue };
                let word = &t[h.keyword..].split_whitespace().next().unwrap_or("");
                if !keywords.contains(word) || h.name.is_empty() {
                    continue;
                }
                declared += 1;
                if named.contains(&(i + 1)) {
                    found += 1;
                } else if lost.len() < 60 {
                    lost.push(format!(
                        "{rel}:{}: {}",
                        i + 1,
                        t.chars().take(70).collect::<String>()
                    ));
                }
            }
            for l in &located {
                let name = &l.symbol.name;
                let checked = matches!(
                    l.symbol.kind.as_str(),
                    "function"
                        | "theorem"
                        | "struct"
                        | "class"
                        | "inductive"
                        | "field"
                        | "constructor"
                        | "namespace"
                        | "axiom"
                );
                let ok = name
                    .split('.')
                    .all(|part| !part.is_empty() && name_len(part) == part.len())
                    || name.starts_with('«');
                if checked && !ok && odd.len() < 60 {
                    odd.push(format!(
                        "{rel}:{}: {} {name:?}",
                        l.symbol.line, l.symbol.kind
                    ));
                }
            }
        }
        eprintln!(
            "{} files; {found} of {declared} declarations outlined",
            files.len()
        );
        eprintln!("slow: {slow:#?}");
        eprintln!("lost ({}): {lost:#?}", lost.len());
        eprintln!("odd names ({}): {odd:#?}", odd.len());
        assert!(slow.is_empty(), "{slow:?}");
    }

    /// A `main` importing a module of its own project runs, against a real
    /// Lake (the one `CLEW_TEST_LAKE` names): the import is built first.
    #[test]
    #[ignore = "needs Lean's lake (CLEW_TEST_LAKE=path/to/lake); run explicitly"]
    fn live_a_main_importing_its_project_runs() {
        let lake = PathBuf::from(
            std::env::var("CLEW_TEST_LAKE").expect("CLEW_TEST_LAKE names the lake binary"),
        );
        let dir = crate::testutil::TempDir::new("lean-run-live");
        let project = dir.to_path_buf();
        let write = |rel: &str, text: &str| {
            let path = project.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        };
        write(
            "lakefile.toml",
            "name = \"demo\"\n\n[[lean_lib]]\nname = \"Demo\"\n",
        );
        if let Ok(toolchain) = std::env::var("CLEW_TEST_LEAN_TOOLCHAIN") {
            write("lean-toolchain", &format!("{toolchain}\n"));
        }
        write("Demo/Basic.lean", "def two : Nat := 2\n");
        let main = write(
            "Main.lean",
            "import Demo.Basic\n\ndef main : IO Unit :=\n  IO.println s!\"two is {two}\"\n",
        );
        let mut said = Vec::new();
        let end = run_main(
            &lake,
            &project,
            &main,
            |line, _| said.push(line.to_string()),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        eprintln!("{said:#?}");
        assert_eq!(end, RunEnd::Exited(0), "{said:?}");
        assert!(said.iter().any(|l| l == "two is 2"), "{said:?}");
    }

    /// A Lean file's `main` runs through `lake env lean --run`, in its
    /// project, its lines said as they come from either stream, its exit
    /// code kept; stopped, it is stopped.
    #[cfg(unix)]
    #[test]
    fn a_main_runs_through_lake_and_says_its_lines() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = crate::testutil::TempDir::new("lean-run");
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let lake = dir.path().join("lake");
        std::fs::write(
            &lake,
            "#!/bin/sh\necho \"args: $*\"\necho \"in: $(pwd)\"\necho oops >&2\nexit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&lake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let file = project.join("Main.lean");
        let mut said = Vec::new();
        let end = run_main(
            &lake,
            &project,
            &file,
            |line, stderr| said.push((line.to_string(), stderr)),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(end, RunEnd::Exited(3));
        assert!(
            said.contains(&(format!("args: env lean --run {}", file.display()), false)),
            "{said:?}"
        );
        let real = std::fs::canonicalize(&project).unwrap();
        assert!(
            said.iter()
                .any(|(l, _)| l == &format!("in: {}", real.display())
                    || l == &format!("in: {}", project.display())),
            "{said:?}"
        );
        assert!(said.contains(&("oops".to_string(), true)), "{said:?}");
        // What it imports of its own is built first; Lean's own is not. A
        // build that fails runs nothing.
        std::fs::write(
            &file,
            "/- A program. -/\nimport Lean\npublic import Proj.Basic Proj.More -- two\n\ndef main : IO Unit := pure ()\nimport Not.This\n",
        )
        .unwrap();
        let mut said = Vec::new();
        let end = run_main(
            &lake,
            &project,
            &file,
            |line, _| said.push(line.to_string()),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(end, RunEnd::Exited(3));
        assert_eq!(said[0], "args: build +Proj.Basic +Proj.More", "{said:?}");
        assert!(!said.iter().any(|l| l.contains("--run")), "{said:?}");
        std::fs::write(&lake, "#!/bin/sh\necho \"args: $*\"\n").unwrap();
        let mut said = Vec::new();
        let end = run_main(
            &lake,
            &project,
            &file,
            |line, _| said.push(line.to_string()),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(end, RunEnd::Exited(0));
        assert_eq!(
            said,
            [
                "args: build +Proj.Basic +Proj.More".to_string(),
                format!("args: env lean --run {}", file.display())
            ]
        );
        // Stopped while it runs.
        std::fs::write(&lake, "#!/bin/sh\necho started\nsleep 30\n").unwrap();
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let end = run_main(
            &lake,
            &project,
            &file,
            |_, _| cancel.store(true, Ordering::SeqCst),
            &cancel,
        )
        .unwrap();
        assert_eq!(end, RunEnd::Stopped);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    fn heads(source: &str) -> Vec<&str> {
        commands(source)
            .into_iter()
            .map(|r| source[r].lines().next().unwrap_or_default())
            .collect()
    }

    /// A command starts each line that starts one: not a continuation, not
    /// inside a block comment, not under a line of attributes alone. The
    /// ranges cover the file.
    #[test]
    fn a_file_splits_where_its_commands_start() {
        let source = "/-\nCopyright.\nAuthors: X\n-/\nmodule\n\npublic import Mathlib.Order.Basic\n\n\
                      assert_not_exists Field\n\n/-- Doc. -/\n@[simp]\ntheorem t :\n    True := by\n  trivial\n\
                      def f : Nat → Nat\n| 0 => 1\n| n + 1 => n\nwhere\n  g := 1\n\
                      instance : Inhabited Nat := ⟨0⟩ -- /- not a comment\n\
                      /- a\n/- nested -/\ndef inside := 1\n-/\nend\n";
        assert_eq!(
            heads(source),
            [
                "/-",
                "module",
                "public import Mathlib.Order.Basic",
                "assert_not_exists Field",
                "/-- Doc. -/",
                "@[simp]",
                "def f : Nat → Nat",
                "instance : Inhabited Nat := ⟨0⟩ -- /- not a comment",
                "/- a",
                "end",
            ]
        );
        let ranges = commands(source);
        assert_eq!(ranges.first().map(|r| r.start), Some(0));
        assert_eq!(ranges.last().map(|r| r.end), Some(source.len()));
        assert!(ranges.windows(2).all(|w| w[0].end == w[1].start));
    }

    #[test]
    fn attributes_alone_are_told_from_a_declaration_after_them() {
        assert!(only_attributes("@[simp]"));
        assert!(only_attributes(
            "@[to_additive (attr := simp) \"a ] b\"] @[inline] -- x"
        ));
        assert!(!only_attributes("@[simp] theorem t : True := trivial"));
        assert!(!only_attributes("@[simp"));
        assert!(!only_attributes("def x := 1"));
    }

    /// The attribute arguments and instance priority the grammar cannot read
    /// are blanked, to the byte; nothing else is.
    #[test]
    fn a_command_is_made_readable_to_the_byte() {
        let cases = [
            (
                "@[to_additive (attr := simp) \"doc\"]\ntheorem t : True := trivial\n",
                "@[to_additive                     ]\ntheorem t : True := trivial\n",
            ),
            (
                "@[simp, inherit_doc A.b, grind =] private def f := 1",
                "@[simp, inherit_doc    , grind  ] private def f := 1",
            ),
            (
                "@[simp] instance (priority := 100) Group.toMonoid : M G := ⟨⟩",
                "@[simp] instance                   Group.toMonoid : M G := ⟨⟩",
            ),
            // A character blanks to as many spaces as it has bytes.
            (
                "@[to_additive «sum_≤»] def x := 1",
                "@[to_additive            ] def x := 1",
            ),
        ];
        for (command, readable_text) in cases {
            let got = readable(command);
            assert_eq!(got, readable_text);
            assert_eq!(got.len(), command.len());
        }
        let plain = "theorem t (h : a = b) : b = a := h.symm";
        assert!(matches!(readable(plain), std::borrow::Cow::Borrowed(_)));
    }

    /// A declaration's head, read from its text: its kind, its whole name,
    /// its statement up to `:=`, `where` or an alternative.
    #[test]
    fn a_declarations_head_is_read_from_its_text() {
        let read = |command: &str| {
            head(command).map(|h| {
                (
                    h.kind,
                    command[h.name].to_string(),
                    command[h.keyword..h.signature_end].trim_end().to_string(),
                )
            })
        };
        let cases = [
            (
                "@[to_additive (attr := simp)]\nprivate theorem a.b_c (x : G) : x = x := rfl\n",
                Some(("theorem", "a.b_c", "theorem a.b_c (x : G) : x = x")),
            ),
            (
                "instance (priority := 100) Group.toMonoid : Monoid G where\n  one := 1\n",
                Some((
                    "instance",
                    "Group.toMonoid",
                    "instance (priority := 100) Group.toMonoid : Monoid G",
                )),
            ),
            (
                "def f : Nat → Nat\n| 0 => 1\n| n + 1 => n\n",
                Some(("function", "f", "def f : Nat → Nat")),
            ),
            (
                "class inductive Decidable (p : Prop) where\n  | isFalse\n",
                Some(("class", "Decidable", "class inductive Decidable (p : Prop)")),
            ),
            (
                "axiom choice : True\n",
                Some(("axiom", "choice", "axiom choice : True")),
            ),
            (
                "theorem «sum ≤» : True := trivial",
                Some(("theorem", "«sum ≤»", "theorem «sum ≤» : True")),
            ),
            (
                "instance : Inhabited Nat := ⟨0⟩",
                Some(("instance", "Inhabited Nat", "instance : Inhabited Nat")),
            ),
            (
                "instance [ToString α] : ToString (List α) where\n  toString := fun _ => \"\"\n",
                Some((
                    "instance",
                    "ToString (List α)",
                    "instance [ToString α] : ToString (List α)",
                )),
            ),
            (
                "example : 2 + 2 = 4 := by\n  decide\n",
                Some(("example", "example", "example : 2 + 2 = 4")),
            ),
            ("instance", None),
            ("assert_not_exists Field", None),
            ("namespace Foo", None),
        ];
        for (command, want) in cases {
            let want = want.map(|(k, n, s)| (k, n.to_string(), s.to_string()));
            assert_eq!(read(command), want, "{command}");
        }
    }

    /// A project that uses Mathlib is told apart from one that does not,
    /// and Mathlib's build missing from one whose build is there — the
    /// manifest's own packages folder, and Mathlib's repository itself.
    #[test]
    fn a_missing_mathlib_build_is_found() {
        let dir = crate::testutil::TempDir::new("lean-mathlib-cache");
        let root = dir.to_path_buf();
        let manifest = |body: &str| std::fs::write(root.join("lake-manifest.json"), body).unwrap();
        assert_eq!(mathlib_build_missing(&root), None, "no manifest");
        manifest(
            r#"{"version": "1.1.0", "packagesDir": ".lake/packages", "packages": [{"name": "batteries"}], "name": "demo"}"#,
        );
        assert_eq!(mathlib_build_missing(&root), None, "no Mathlib");
        manifest(
            r#"{"version": "1.1.0", "packagesDir": ".lake/packages", "packages": [{"name": "mathlib", "scope": "leanprover-community"}], "name": "demo"}"#,
        );
        let build = root.join(".lake/packages/mathlib/.lake/build");
        assert_eq!(mathlib_build_missing(&root), Some(build.clone()));
        std::fs::create_dir_all(build.join("lib/lean")).unwrap();
        std::fs::write(build.join("lib/lean/Mathlib.olean"), "x").unwrap();
        assert_eq!(mathlib_build_missing(&root), None, "built");
        // Mathlib's own repository, an older Lake's packages folder.
        manifest(
            r#"{"version": 7, "packagesDir": "lake-packages", "packages": [], "name": "mathlib"}"#,
        );
        assert_eq!(
            mathlib_build_missing(&root),
            Some(root.join(".lake/build")),
            "Mathlib's own build"
        );
        manifest("not json");
        assert_eq!(mathlib_build_missing(&root), None);
    }

    /// A Lean file's Lake project is the nearest folder above it with a
    /// lakefile, inside the opened folder: one nested in another is its
    /// own; a file in none is the opened folder's.
    #[test]
    fn a_lean_files_lake_project_is_the_nearest_with_a_lakefile() {
        let dir = crate::testutil::TempDir::new("lean-lake-root");
        let root = dir.to_path_buf();
        let at = |rel: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "").unwrap();
            path
        };
        at("verification/lakefile.lean");
        at("verification/extracted/lakefile.toml");
        let proof = at("verification/Proofs/Trust.lean");
        let extracted = at("verification/extracted/Gen/Funs.lean");
        let loose = at("scratch/Try.lean");
        assert_eq!(lake_root(&root, &proof), Some(root.join("verification")));
        assert_eq!(
            lake_root(&root, &extracted),
            Some(root.join("verification/extracted"))
        );
        assert_eq!(lake_root(&root, &loose), Some(root.clone()));
        // A dependency's file is the project's that holds its checkout, not
        // the dependency's own; Lake 4's folder too.
        at("verification/.lake/packages/mathlib/lakefile.lean");
        let dependency = at("verification/.lake/packages/mathlib/Mathlib/Logic/Basic.lean");
        assert_eq!(
            lake_root(&root, &dependency),
            Some(root.join("verification"))
        );
        at("verification/lake-packages/std/lakefile.lean");
        let old = at("verification/lake-packages/std/Std/Data.lean");
        assert_eq!(lake_root(&root, &old), Some(root.join("verification")));
        // Opened itself, a dependency's checkout is its own project.
        let mathlib = root.join("verification/.lake/packages/mathlib");
        assert_eq!(lake_root(&mathlib, &dependency), Some(mathlib.clone()));
        // A file outside the opened folder (the toolchain's sources): none.
        assert_eq!(lake_root(&root.join("verification"), &loose), None);
        // Not past the opened folder, even where a lakefile is above it: its
        // lakefile is code Lake runs, of a folder not opened.
        let inner = root.join("scratch");
        at("lakefile.lean");
        assert_eq!(lake_root(&inner, &loose), Some(inner));
        assert_eq!(lake_root(&root, &loose), Some(root));
    }

    #[test]
    fn an_empty_file_has_no_commands() {
        assert!(commands("").is_empty());
        assert_eq!(commands("def x := 1"), vec![0..10]);
    }
}
