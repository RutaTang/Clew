//! C and C++ macro expansion at a point: what the preprocessor makes of the
//! macro under the cursor, or of a whole line, from the `#define`s in
//! effect there — the file's own above it and those of the headers it
//! includes (found next to it, in the include directories its compile
//! command names, or in the project's `include/`), less what `#undef`s
//! take away and what the groups the preprocessor drops would have
//! defined ([`crate::inactive`]).
//!
//! The expansion follows the standard's algorithm (C11 6.10.3) as
//! Prosser's hide sets put it: arguments are expanded before they are
//! substituted, except next to `#` and `##`; the result is rescanned; a
//! macro is never expanded again inside its own expansion. `__VA_OPT__`,
//! GNU's `, ## __VA_ARGS__` and named variadics (`args...`) are understood.
//! Headers outside the project (the system's, a library's not in the
//! compile command) are not read, so their macros stay as written.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::inactive::{self, Target};

/// What a macro, or a line, expands to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    /// The macro expanded; empty when a whole line was.
    pub name: String,
    /// What was expanded, as written: `MAX(a, b)`, or the line.
    pub invocation: String,
    /// What the preprocessor makes of it.
    pub expanded: String,
    /// Where the macro is defined (1-based line), when one was named.
    pub defined: Option<(PathBuf, usize)>,
    /// Its `#define`, as written.
    pub definition: String,
    /// Whether the expansion was cut short (a runaway, or a huge one).
    pub truncated: bool,
}

/// What a file is compiled with, as far as expanding needs.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// Where `#include`s are looked for, after the including file's
    /// directory (for the quoted form).
    pub include_dirs: Vec<PathBuf>,
    /// Macros the command line defines (`-DNAME=value`), as `(name, body)`.
    pub predefined: Vec<(String, String)>,
    /// The platform the groups of `#if`s are read for.
    pub target: Option<Target>,
}

impl Context {
    /// The context for `file` of the project at `root`: the `-I`, `-D` and
    /// `-U` of its entry in a compilation database (`compile_commands.json`
    /// at the root or in a build directory) or of `compile_flags.txt`, then
    /// the project's own `include/`, `inc/` and `src/`.
    pub fn for_file(root: &Path, file: &Path, read: &dyn Fn(&Path) -> Option<String>) -> Context {
        let mut cx = Context::default();
        let mut undefined: Vec<String> = Vec::new();
        if let Some((dir, args)) = compile_args(root, file, read) {
            let mut it = args.iter().peekable();
            while let Some(a) = it.next() {
                let mut value = |flag: &str| -> Option<String> {
                    match a.strip_prefix(flag) {
                        Some("") => it.next().cloned(),
                        Some(v) => Some(v.to_string()),
                        None => None,
                    }
                };
                if let Some(d) = value("-isystem")
                    .or_else(|| value("-iquote"))
                    .or_else(|| value("-I"))
                {
                    cx.include_dirs.push(dir.join(d));
                } else if let Some(d) = value("-D") {
                    let (name, body) = d.split_once('=').unwrap_or((d.as_str(), "1"));
                    cx.predefined.push((name.to_string(), body.to_string()));
                } else if let Some(u) = value("-U") {
                    undefined.push(u);
                }
            }
        }
        cx.predefined.retain(|(n, _)| !undefined.contains(n));
        for d in ["include", "inc", "src"] {
            cx.include_dirs.push(root.join(d));
        }
        cx.include_dirs.push(root.to_path_buf());
        cx
    }
}

/// The working directory and arguments `file` is compiled with.
fn compile_args(
    root: &Path,
    file: &Path,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Option<(PathBuf, Vec<String>)> {
    for dir in [
        "",
        "build",
        "out",
        "cmake-build-debug",
        "cmake-build-release",
        "builddir",
    ] {
        let db = root.join(dir).join("compile_commands.json");
        let Some(text) = read(&db) else {
            continue;
        };
        let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        for e in &entries {
            let directory = PathBuf::from(e.get("directory").and_then(Value::as_str).unwrap_or(""));
            let Some(f) = e.get("file").and_then(Value::as_str) else {
                continue;
            };
            if normal(&directory.join(f)) != normal(file) {
                continue;
            }
            let args: Vec<String> = match e.get("arguments").and_then(Value::as_array) {
                Some(a) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                None => split_command(e.get("command").and_then(Value::as_str).unwrap_or("")),
            };
            return Some((directory, args));
        }
    }
    let flags = read(&root.join("compile_flags.txt"))?;
    Some((
        root.to_path_buf(),
        flags
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
    ))
}

/// `a/./b/../c` → `a/c`, without touching the disk.
fn normal(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// A shell command line split into words, quotes and backslashes honoured.
fn split_command(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut any = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, ' ' | '\t' | '\n') => {
                if any {
                    out.push(std::mem::take(&mut word));
                    any = false;
                }
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => word.push(c),
            (_, '\\') => {
                if let Some(n) = chars.next() {
                    word.push(n);
                }
                any = true;
            }
            (_, c) => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(word);
    }
    out
}

// -- tokens -----------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tok {
    text: String,
    /// Whitespace before it.
    space: bool,
    /// The macros it came out of, which may not expand it again.
    hide: BTreeSet<String>,
}

impl Tok {
    fn new(text: &str, space: bool) -> Tok {
        Tok {
            text: text.to_string(),
            space,
            hide: BTreeSet::new(),
        }
    }

    fn ident(&self) -> bool {
        self.text
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
            && !self.text.ends_with(['"', '\''])
    }

    fn literal(&self) -> bool {
        self.text.ends_with(['"', '\'']) || self.text.starts_with(['"', '\''])
    }
}

const PUNCTS: &[&str] = &[
    "%:%:", "...", "<<=", ">>=", "->*", "<=>", "->", "++", "--", "<<", ">>", "<=", ">=", "==",
    "!=", "&&", "||", "*=", "/=", "%=", "+=", "-=", "&=", "^=", "|=", "##", "::", ".*", "%:",
];

/// Where a string or character literal whose quote is at `q` ends.
fn literal_end(b: &[u8], q: usize, raw: bool) -> usize {
    if raw && b[q] == b'"' {
        // R"delim( … )delim"
        let open = b[q..].iter().position(|c| *c == b'(').map(|p| q + p);
        if let Some(open) = open {
            let delim = &b[q + 1..open];
            let mut i = open + 1;
            while i < b.len() {
                if b[i] == b')'
                    && b[i + 1..].starts_with(delim)
                    && b.get(i + 1 + delim.len()) == Some(&b'"')
                {
                    return i + delim.len() + 2;
                }
                i += 1;
            }
            return b.len();
        }
    }
    let quote = b[q];
    let mut i = q + 1;
    while i < b.len() && b[i] != quote && b[i] != b'\n' {
        i += if b[i] == b'\\' { 2 } else { 1 };
    }
    (i + 1).min(b.len())
}

/// The preprocessing tokens of comment-free `src`.
fn lex(src: &str) -> Vec<Tok> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut space = false;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            space = true;
            i += 1;
            continue;
        }
        let start = i;
        if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 {
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80)
            {
                i += 1;
            }
            let word = &src[start..i];
            if i < b.len()
                && matches!(b[i], b'"' | b'\'')
                && matches!(
                    word,
                    "L" | "u" | "U" | "u8" | "R" | "LR" | "uR" | "UR" | "u8R"
                )
            {
                i = literal_end(b, i, word.ends_with('R'));
            }
        } else if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            i += 1;
            while i < b.len() {
                if matches!(b[i], b'e' | b'E' | b'p' | b'P')
                    && matches!(b.get(i + 1), Some(b'+' | b'-'))
                {
                    i += 2;
                } else if b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'.' | b'\'') {
                    i += 1;
                } else {
                    break;
                }
            }
        } else if c == b'"' || c == b'\'' {
            i = literal_end(b, i, false);
        } else {
            i += PUNCTS
                .iter()
                .find(|p| src[i..].starts_with(**p))
                .map_or(1, |p| p.len());
        }
        out.push(Tok::new(&src[start..i], space));
        space = false;
    }
    out
}

/// `text` with its comments blanked to spaces, newlines kept.
fn blank_comments(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => i = literal_end(b, i, i > 0 && b[i - 1] == b'R'),
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let end = text[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 2);
                for c in &mut out[i..end] {
                    if *c != b'\n' {
                        *c = b' ';
                    }
                }
                i = end;
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// A logical line: physical lines joined at their trailing backslashes.
struct Logical {
    /// 0-based, its first physical line.
    first: usize,
    last: usize,
    text: String,
    /// Where each physical line starts in `text`.
    starts: Vec<usize>,
}

fn logical_lines(text: &str) -> Vec<Logical> {
    let mut out: Vec<Logical> = Vec::new();
    let mut open = false;
    for (n, line) in text.lines().enumerate() {
        let (body, continues) = match line.strip_suffix('\\') {
            Some(b) => (b, true),
            None => (line, false),
        };
        if open && let Some(l) = out.last_mut() {
            l.starts.push(l.text.len());
            l.text.push_str(body);
            l.last = n;
        } else {
            out.push(Logical {
                first: n,
                last: n,
                text: body.to_string(),
                starts: vec![0],
            });
        }
        open = continues;
    }
    out
}

// -- definitions ------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Def {
    /// `None` for an object-like macro.
    params: Option<Vec<String>>,
    /// Whether the last parameter takes the rest of the arguments.
    variadic: bool,
    body: Vec<Tok>,
    file: PathBuf,
    line: usize,
    written: String,
}

/// `NAME(params) body` (the text after `#define`).
fn parse_define(rest: &str, file: &Path, line: usize) -> Option<(String, Def)> {
    let rest = rest.trim_start();
    let len = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(rest.len());
    if len == 0 {
        return None;
    }
    let name = &rest[..len];
    let after = &rest[len..];
    let (params, variadic, body) = match after.strip_prefix('(') {
        Some(inner) => {
            let close = inner.find(')')?;
            let mut params = Vec::new();
            let mut variadic = false;
            for p in inner[..close]
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
            {
                match p.strip_suffix("...") {
                    Some("") => {
                        params.push("__VA_ARGS__".to_string());
                        variadic = true;
                    }
                    Some(named) => {
                        params.push(named.trim().to_string());
                        variadic = true;
                    }
                    None => params.push(p.to_string()),
                }
            }
            (Some(params), variadic, &inner[close + 1..])
        }
        None => (None, false, after),
    };
    let mut body = lex(body);
    if let Some(first) = body.first_mut() {
        first.space = false;
    }
    let written = format!(
        "#define {}",
        rest.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    Some((
        name.to_string(),
        Def {
            params,
            variadic,
            body,
            file: file.to_path_buf(),
            line,
            written,
        },
    ))
}

/// Most headers deep an `#include` chain is followed.
const MAX_INCLUDE_DEPTH: usize = 24;

struct Collector<'a> {
    defs: HashMap<String, Def>,
    cx: &'a Context,
    target: Target,
    read: &'a dyn Fn(&Path) -> Option<String>,
    seen: HashSet<PathBuf>,
}

impl Collector<'_> {
    /// Reads the `#define`s and `#undef`s of `path` (its text `text`) in
    /// order, following `#include`s, through 0-based line `upto`.
    fn collect(&mut self, path: &Path, text: &str, upto: Option<usize>, lang: &str, depth: usize) {
        let inactive = inactive::inactive_lines(text, lang, &self.target);
        for l in logical_lines(&blank_comments(text)) {
            if upto.is_some_and(|u| l.first > u) {
                break;
            }
            if inactive.contains(&l.first) {
                continue;
            }
            let Some(d) = l.text.trim_start().strip_prefix('#') else {
                continue;
            };
            let d = d.trim_start();
            let word_len = d
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(d.len());
            let (word, rest) = d.split_at(word_len);
            match word {
                "define" => {
                    if let Some((name, def)) = parse_define(rest, path, l.first + 1) {
                        self.defs.insert(name, def);
                    }
                }
                "undef" => {
                    self.defs.remove(rest.trim());
                }
                "include" | "include_next" | "import" if depth < MAX_INCLUDE_DEPTH => {
                    let spec = rest.trim();
                    let (name, quoted) = match (spec.strip_prefix('"'), spec.strip_prefix('<')) {
                        (Some(s), _) => (s.split('"').next().unwrap_or(""), true),
                        (_, Some(s)) => (s.split('>').next().unwrap_or(""), false),
                        _ => continue,
                    };
                    let here = path.parent().map(Path::to_path_buf);
                    let bases = quoted
                        .then_some(here)
                        .flatten()
                        .into_iter()
                        .chain(self.cx.include_dirs.iter().cloned());
                    for base in bases {
                        let candidate = normal(&base.join(name));
                        if let Some(header) = (self.read)(&candidate) {
                            if self.seen.insert(candidate.clone()) {
                                self.collect(&candidate, &header, None, lang, depth + 1);
                            }
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

// -- expansion --------------------------------------------------------------------------------------

/// Most macro expansions one request makes, and most tokens it produces:
/// past either, the rest is left as it stands.
const MAX_STEPS: usize = 20_000;
const MAX_TOKENS: usize = 100_000;

struct Expander<'a> {
    defs: &'a HashMap<String, Def>,
    steps: usize,
    truncated: bool,
    file: String,
    line: usize,
}

impl Expander<'_> {
    fn expand(&mut self, mut input: VecDeque<Tok>) -> Vec<Tok> {
        let mut out = Vec::new();
        while let Some(t) = input.pop_front() {
            if self.steps >= MAX_STEPS || out.len() + input.len() > MAX_TOKENS {
                self.truncated = true;
                out.push(t);
                out.extend(input);
                break;
            }
            if !t.ident() || t.hide.contains(&t.text) {
                out.push(t);
                continue;
            }
            match t.text.as_str() {
                "__LINE__" => {
                    out.push(Tok {
                        text: self.line.to_string(),
                        ..t
                    });
                    continue;
                }
                "__FILE__" => {
                    out.push(Tok {
                        text: format!("\"{}\"", self.file),
                        ..t
                    });
                    continue;
                }
                _ => {}
            }
            let Some(def) = self.defs.get(&t.text) else {
                out.push(t);
                continue;
            };
            let mut hide = t.hide.clone();
            let args = match &def.params {
                None => Vec::new(),
                Some(_) => {
                    let Some((args, rparen)) = take_args(&mut input, def) else {
                        out.push(t);
                        continue;
                    };
                    hide = hide.intersection(&rparen.hide).cloned().collect();
                    args
                }
            };
            hide.insert(t.text.clone());
            self.steps += 1;
            let mut body = self.subst(def, &args, &hide);
            if let Some(first) = body.first_mut() {
                first.space = t.space;
            }
            for x in body.into_iter().rev() {
                input.push_front(x);
            }
        }
        out
    }

    /// `def`'s body with `args` in place of its parameters, each token
    /// hidden from the macros in `hide`.
    fn subst(&mut self, def: &Def, args: &[Vec<Tok>], hide: &BTreeSet<String>) -> Vec<Tok> {
        let params: &[String] = def.params.as_deref().unwrap_or(&[]);
        let param = |t: &Tok| {
            t.ident()
                .then(|| params.iter().position(|p| *p == t.text))
                .flatten()
        };
        let body = &def.body;
        let va = def.variadic.then(|| args.last()).flatten();
        let mut out: Vec<Tok> = Vec::new();
        let mut i = 0;
        while i < body.len() {
            let t = &body[i];
            // `__VA_OPT__(x)`: x when variadic arguments were passed.
            if t.text == "__VA_OPT__"
                && def.variadic
                && body.get(i + 1).is_some_and(|p| p.text == "(")
            {
                let close = group_end(body, i + 1).unwrap_or(body.len() - 1);
                if va.is_some_and(|v| !v.is_empty()) {
                    let inner = Def {
                        body: body[i + 2..close].to_vec(),
                        ..def.clone()
                    };
                    let mut part = self.subst(&inner, args, &BTreeSet::new());
                    if let Some(f) = part.first_mut() {
                        f.space = t.space;
                    }
                    out.extend(part);
                }
                i = close + 1;
                continue;
            }
            // `#x`: the argument, spelled as a string.
            if t.text == "#"
                && def.params.is_some()
                && let Some(p) = body.get(i + 1).and_then(param)
            {
                out.push(Tok {
                    text: stringify(&args[p]),
                    space: t.space,
                    hide: BTreeSet::new(),
                });
                i += 2;
                continue;
            }
            // `## y`: pasted onto what comes before.
            if t.text == "##" && i + 1 < body.len() {
                let next = &body[i + 1];
                match param(next) {
                    Some(p) if args[p].is_empty() => {
                        // `, ## __VA_ARGS__` with none: the comma goes too.
                        if def.variadic
                            && p == params.len() - 1
                            && out.last().is_some_and(|l| l.text == ",")
                        {
                            out.pop();
                        }
                    }
                    // `, ## __VA_ARGS__` with some: GNU keeps the comma as it is.
                    Some(p)
                        if def.variadic
                            && p == params.len() - 1
                            && out.last().is_some_and(|l| l.text == ",") =>
                    {
                        out.extend(args[p].iter().cloned());
                    }
                    Some(p) => {
                        paste(&mut out, args[p][0].clone());
                        out.extend(args[p][1..].iter().cloned());
                    }
                    None => paste(&mut out, next.clone()),
                }
                i += 2;
                continue;
            }
            if let Some(p) = param(t) {
                let arg = &args[p];
                if body.get(i + 1).is_some_and(|n| n.text == "##") {
                    // Pasted: the argument as written, not expanded.
                    if arg.is_empty() {
                        // An empty one pastes nothing: the token after `##`
                        // stands alone (or goes too, when it is empty).
                        if let Some(n) = body.get(i + 2) {
                            match param(n) {
                                Some(q) => {
                                    let mut a = args[q].clone();
                                    if let Some(f) = a.first_mut() {
                                        f.space = t.space;
                                    }
                                    out.extend(a);
                                }
                                None => out.push(Tok {
                                    space: t.space,
                                    ..n.clone()
                                }),
                            }
                        }
                        i += 3;
                        continue;
                    }
                    let mut a = arg.clone();
                    a[0].space = t.space;
                    out.extend(a);
                    i += 1;
                    continue;
                }
                let mut expanded = self.expand(arg.iter().cloned().collect());
                if let Some(f) = expanded.first_mut() {
                    f.space = t.space;
                }
                out.extend(expanded);
                i += 1;
                continue;
            }
            out.push(t.clone());
            i += 1;
        }
        for t in &mut out {
            t.hide.extend(hide.iter().cloned());
        }
        out
    }
}

/// The index of the `)` closing the `(` at `open`.
fn group_end(toks: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0;
    for (k, t) in toks.iter().enumerate().skip(open) {
        match t.text.as_str() {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

/// The arguments of a call of `def` whose `(` is `input`'s front, and its
/// `)` — taken off `input` only when they fit the macro.
fn take_args(input: &mut VecDeque<Tok>, def: &Def) -> Option<(Vec<Vec<Tok>>, Tok)> {
    if input.front().is_none_or(|t| t.text != "(") {
        return None;
    }
    let toks: Vec<Tok> = input.iter().cloned().collect();
    let end = group_end(&toks, 0)?;
    let mut args: Vec<Vec<Tok>> = vec![Vec::new()];
    let mut depth = 0;
    for t in &toks[1..end] {
        match t.text.as_str() {
            "(" => depth += 1,
            ")" => depth -= 1,
            "," if depth == 0 => {
                args.push(Vec::new());
                continue;
            }
            _ => {}
        }
        args.last_mut().unwrap().push(t.clone());
    }
    let n = def.params.as_ref().map_or(0, Vec::len);
    let args = if n == 0 {
        (args.len() == 1 && args[0].is_empty()).then(Vec::new)?
    } else if def.variadic {
        if args.len() + 1 < n {
            return None;
        }
        if args.len() + 1 == n {
            args.push(Vec::new());
        }
        // The variadic part keeps its commas.
        let rest = args.split_off(n - 1);
        let mut joined = Vec::new();
        for (k, a) in rest.into_iter().enumerate() {
            if k > 0 {
                joined.push(Tok::new(",", false));
            }
            joined.extend(a);
        }
        args.push(joined);
        args
    } else {
        (args.len() == n).then_some(args)?
    };
    let rparen = toks[end].clone();
    input.drain(..=end);
    Some((args, rparen))
}

/// `a ## b`: the last token of `out` and `next` made one.
fn paste(out: &mut Vec<Tok>, next: Tok) {
    match out.pop() {
        Some(last) => {
            let hide = last.hide.intersection(&next.hide).cloned().collect();
            out.push(Tok {
                text: format!("{}{}", last.text, next.text),
                space: last.space,
                hide,
            });
        }
        None => out.push(next),
    }
}

/// `#x`: an argument spelled as a string literal.
fn stringify(arg: &[Tok]) -> String {
    let mut s = String::from("\"");
    for (i, t) in arg.iter().enumerate() {
        if i > 0 && t.space {
            s.push(' ');
        }
        if t.literal() {
            for c in t.text.chars() {
                if matches!(c, '"' | '\\') {
                    s.push('\\');
                }
                s.push(c);
            }
        } else {
            s.push_str(&t.text);
        }
    }
    s.push('"');
    s
}

/// Tokens back to text, a space where there was one — or where two would
/// otherwise lex as something else (`+` `+` is not `++`).
fn render(toks: &[Tok]) -> String {
    let mut s = String::new();
    for (i, t) in toks.iter().enumerate() {
        if i > 0 {
            let prev = &toks[i - 1].text;
            let merged = lex(&format!("{prev}{}", t.text));
            if t.space || merged.first().is_none_or(|m| m.text != *prev) {
                s.push(' ');
            }
        }
        s.push_str(&t.text);
    }
    s
}

/// The language key `inactive` reads a C or C++ file by.
fn lang_of(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("c" | "h") => "c",
        _ => "cpp",
    }
}

/// What the macro under byte `col` of 0-based `line` in `path` (its text
/// `text`) expands to there, or the whole line when the cursor is on no
/// macro. `None` when nothing there is a macro the file can see.
pub fn expand_at(
    path: &Path,
    text: &str,
    line: usize,
    col: usize,
    cx: &Context,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Option<Expansion> {
    let target = cx.target.clone().unwrap_or_else(Target::host);
    let mut c = Collector {
        defs: HashMap::new(),
        cx,
        target,
        read,
        seen: HashSet::new(),
    };
    for (name, body) in &cx.predefined {
        if let Some((n, d)) =
            parse_define(&format!("{name} {body}"), Path::new("<command line>"), 0)
        {
            c.defs.insert(n, d);
        }
    }
    c.seen.insert(normal(path));
    c.collect(path, text, Some(line), lang_of(path), 0);
    let defs = c.defs;

    let lines = logical_lines(&blank_comments(text));
    let at = lines
        .iter()
        .position(|l| l.first <= line && line <= l.last)?;
    let logical = &lines[at];
    let pos = logical
        .starts
        .get(line - logical.first)
        .copied()
        .unwrap_or(0)
        + col;
    let src = logical.text.as_str();
    // The identifier under the cursor.
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let pos = pos.min(src.len());
    let start = src[..pos]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(pos, |(i, _)| i);
    let end = src[start..]
        .find(|c: char| !is_word(c))
        .map_or(src.len(), |e| start + e);
    let word = &src[start..end];
    let mut ex = Expander {
        defs: &defs,
        steps: 0,
        truncated: false,
        file: path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("")
            .to_string(),
        line: line + 1,
    };
    let directive = src.trim_start().starts_with('#');
    match defs.get(word) {
        Some(def) => {
            // The invocation and what follows it, on to the next lines for
            // arguments that span them.
            let mut rest = src[start..].to_string();
            for l in lines.iter().skip(at + 1).take(64) {
                if l.text.trim_start().starts_with('#') {
                    break;
                }
                rest.push('\n');
                rest.push_str(&l.text);
            }
            let toks = lex(&rest);
            let mut invocation: Vec<Tok> = match &def.params {
                Some(params) => match (
                    directive,
                    group_end(&toks, 1).filter(|_| toks.get(1).is_some_and(|t| t.text == "(")),
                ) {
                    (false, Some(close)) => toks[..=close].to_vec(),
                    // At its definition, or named without a call: shown
                    // over its own parameters.
                    _ => {
                        let mut call = vec![toks[0].clone(), Tok::new("(", false)];
                        for (k, p) in params.iter().enumerate() {
                            if k > 0 {
                                call.push(Tok::new(",", false));
                            }
                            let name = if p == "__VA_ARGS__" { "..." } else { p };
                            let mut t = Tok::new(name, k > 0);
                            t.hide.insert(p.clone());
                            call.push(t);
                        }
                        call.push(Tok::new(")", false));
                        call
                    }
                },
                None => vec![toks[0].clone()],
            };
            if let Some(first) = invocation.first_mut() {
                first.space = false;
            }
            let mut expanded = ex.expand(invocation.iter().cloned().collect());
            // An object-like macro naming a function-like one takes the call
            // after it: `#define f g` … `f(1)`.
            let mut used = invocation.len();
            while let Some(last) = expanded.last()
                && last.ident()
                && !last.hide.contains(&last.text)
                && defs.get(&last.text).is_some_and(|d| d.params.is_some())
                && toks.get(used).is_some_and(|t| t.text == "(")
                && let Some(close) = group_end(&toks, used)
            {
                invocation.extend(toks[used..=close].iter().cloned());
                used = close + 1;
                expanded = ex.expand(invocation.iter().cloned().collect());
            }
            Some(Expansion {
                name: word.to_string(),
                invocation: render(&invocation),
                expanded: render(&expanded),
                defined: (def.line > 0).then(|| (def.file.clone(), def.line)),
                definition: def.written.clone(),
                truncated: ex.truncated,
            })
        }
        None if !directive => {
            let toks = lex(src);
            let expanded = ex.expand(toks.iter().cloned().collect());
            (expanded != toks).then(|| Expansion {
                name: String::new(),
                invocation: render(&toks),
                expanded: render(&expanded),
                defined: None,
                definition: String::new(),
                truncated: ex.truncated,
            })
        }
        None => None,
    }
}

/// Largest header [`expand_in_project`] reads.
const MAX_HEADER_BYTES: u64 = 4 << 20;

/// [`expand_at`] for the file `rel` of the project at `root`, reading only
/// what is inside the project — its compilation database, the file and the
/// headers it includes — and answered in the protocol's form, with the
/// defining file relative to the project.
pub fn expand_in_project(
    root: &Path,
    rel: &str,
    line: usize,
    col: usize,
    target: &Target,
) -> Result<Option<clew_protocol::MacroExpansion>, String> {
    let abs = crate::confine::confine(root, rel).map_err(|e| format!("{rel}: {e}"))?;
    let text = std::fs::read_to_string(&abs).map_err(|e| format!("{rel}: {e}"))?;
    let root = std::fs::canonicalize(root).map_err(|e| format!("the project: {e}"))?;
    let read = |p: &Path| -> Option<String> {
        let real = std::fs::canonicalize(p).ok()?;
        let meta = std::fs::metadata(&real).ok()?;
        (real.starts_with(&root) && meta.is_file() && meta.len() <= MAX_HEADER_BYTES)
            .then(|| std::fs::read_to_string(&real).ok())
            .flatten()
    };
    let mut cx = Context::for_file(&root, &abs, &read);
    cx.target = Some(target.clone());
    let Some(e) = expand_at(&abs, &text, line, col, &cx, &read) else {
        return Ok(None);
    };
    // The defining file, as the project names it.
    let (file, line) = match &e.defined {
        Some((f, l)) => match std::fs::canonicalize(f)
            .ok()
            .and_then(|c| c.strip_prefix(&root).ok().map(Path::to_path_buf))
        {
            Some(r) => (r.to_string_lossy().replace('\\', "/"), *l),
            None => (String::new(), 0),
        },
        None => (String::new(), 0),
    };
    Ok(Some(clew_protocol::MacroExpansion {
        name: e.name,
        invocation: e.invocation,
        expanded: e.expanded,
        file,
        line,
        definition: e.definition,
        truncated: e.truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &[(&str, &str)]) -> impl Fn(&Path) -> Option<String> {
        let map: HashMap<PathBuf, String> = list
            .iter()
            .map(|(p, t)| (PathBuf::from(p), t.to_string()))
            .collect();
        move |p: &Path| map.get(p).cloned()
    }

    /// What `text`'s line `line` expands to at the first `at`.
    fn at(text: &str, line: usize, at: &str) -> Option<Expansion> {
        let col = text.lines().nth(line).unwrap().find(at).unwrap();
        expand_at(
            Path::new("/p/a.c"),
            text,
            line,
            col,
            &Context::default(),
            &files(&[]),
        )
    }

    fn expanded(text: &str, line: usize, word: &str) -> String {
        at(text, line, word).map(|e| e.expanded).unwrap_or_default()
    }

    #[test]
    fn object_and_function_like_macros() {
        let src = "#define N 10\n#define SQ(x) ((x) * (x))\n#define MAX(a, b) ((a) > (b) ? (a) : (b))\nint k = MAX(SQ(N), 3) + N;\n";
        let e = at(src, 3, "MAX").unwrap();
        assert_eq!(e.name, "MAX");
        assert_eq!(e.invocation, "MAX(SQ(N), 3)");
        assert_eq!(
            e.expanded,
            "((((10) * (10))) > (3) ? (((10) * (10))) : (3))"
        );
        assert_eq!(e.defined, Some((PathBuf::from("/p/a.c"), 3)));
        assert_eq!(e.definition, "#define MAX(a, b) ((a) > (b) ? (a) : (b))");
        assert_eq!(expanded(src, 3, "N;"), "10");
        // On no macro: the whole line.
        let line = at(src, 3, "int").unwrap();
        assert_eq!(line.name, "");
        assert_eq!(
            line.expanded,
            "int k = ((((10) * (10))) > (3) ? (((10) * (10))) : (3)) + 10;"
        );
        assert!(at("int x = 1;\n", 0, "x").is_none());
    }

    #[test]
    fn the_standards_own_examples() {
        // C11 6.10.3.5, example 3.
        let src = "#define x 3\n#define f(a) f(x * (a))\n#undef x\n#define x 2\n#define g f\n#define z z[0]\n#define h g(~\n#define m(a) a(w)\n#define w 0,1\n#define t(a) a\n#define p() int\n#define q(x) x\n#define r(x,y) x ## y\n#define str(x) # x\nf(y+1) + f(f(z)) % t(t(g)(0) + t)(1);\n";
        assert_eq!(expanded(src, 14, "f(y"), "f(2 * (y+1))");
        assert_eq!(
            at(src, 14, "%").unwrap().expanded,
            "f(2 * (y+1)) + f(2 * (f(2 * (z[0])))) % f(2 * (0)) + t(1);"
        );
        // Recursion stops at the macro itself: `#define z z[0]`.
        assert_eq!(expanded(src, 14, "z"), "z[0]");
        // Example 4: # and ##.
        let src = "#define str(s) # s\n#define xstr(s) str(s)\n#define debug(s, t) printf(\"x\" # s \"= %d, x\" # t \"= %s\", x ## s, x ## t)\n#define INCFILE(n) vers ## n\n#define glue(a, b) a ## b\n#define xglue(a, b) glue(a, b)\n#define HIGHLOW \"hello\"\n#define LOW LOW \", world\"\ndebug(1, 2);\nglue(HIGH, LOW);\nxglue(HIGH, LOW)\nxstr(INCFILE(2).h)\nstr(strncmp(\"abc\\0d\", \"abc\", '\\4') == 0)\n";
        assert_eq!(
            expanded(src, 8, "debug"),
            "printf(\"x\" \"1\" \"= %d, x\" \"2\" \"= %s\", x1, x2)"
        );
        assert_eq!(expanded(src, 9, "glue"), "\"hello\"");
        assert_eq!(expanded(src, 10, "xglue"), "\"hello\" \", world\"");
        assert_eq!(expanded(src, 11, "xstr"), "\"vers2.h\"");
        assert_eq!(
            expanded(src, 12, "str"),
            "\"strncmp(\\\"abc\\\\0d\\\", \\\"abc\\\", '\\\\4') == 0\""
        );
    }

    #[test]
    fn variadics_and_empty_pastes() {
        let src = "#define LOG(fmt, ...) printf(fmt, ## __VA_ARGS__)\n#define F(...) f(0 __VA_OPT__(,) __VA_ARGS__)\n#define E(args...) e(args)\n#define CAT(a, b) a ## b\nLOG(\"x\");\nLOG(\"x %d %d\", 1, 2);\nF();\nF(a, b);\nE(1, 2, 3);\nCAT(, y);\nCAT(x, );\n";
        assert_eq!(expanded(src, 4, "LOG"), "printf(\"x\")");
        assert_eq!(expanded(src, 5, "LOG"), "printf(\"x %d %d\", 1, 2)");
        assert_eq!(expanded(src, 6, "F"), "f(0)");
        assert_eq!(expanded(src, 7, "F"), "f(0 , a, b)");
        assert_eq!(expanded(src, 8, "E"), "e(1, 2, 3)");
        assert_eq!(expanded(src, 9, "CAT"), "y");
        assert_eq!(expanded(src, 10, "CAT"), "x");
    }

    #[test]
    fn at_its_definition_over_its_parameters() {
        let src = "#define MIN(a, b) ((a) < (b) ? (a) : (b))\n";
        let e = at(src, 0, "MIN").unwrap();
        assert_eq!(e.invocation, "MIN(a, b)");
        assert_eq!(e.expanded, "((a) < (b) ? (a) : (b))");
    }

    #[test]
    fn arguments_across_lines_and_object_macros_naming_calls() {
        let src = "#define ADD(a, b) ((a) + (b))\n#define plus ADD\nint v = ADD(1,\n            2);\nint w = plus(3, 4);\n";
        assert_eq!(expanded(src, 2, "ADD"), "((1) + (2))");
        let e = at(src, 4, "plus").unwrap();
        assert_eq!(e.invocation, "plus(3, 4)");
        assert_eq!(e.expanded, "((3) + (4))");
    }

    #[test]
    fn defines_from_headers_undefs_and_dropped_groups() {
        let read = files(&[
            (
                "/p/include/util.h",
                "#ifndef UTIL_H\n#define UTIL_H\n#define KB(n) ((n) * 1024)\n#include \"more.h\"\n#endif\n",
            ),
            (
                "/p/include/more.h",
                "#define PAGE KB(4) /* a page */\n#include \"util.h\"\n",
            ),
            ("/p/src/a.c", ""),
        ]);
        let src = "#include <util.h>\n#if 0\n#define PAGE 1\n#endif\n#ifdef _WIN32\n#define PAGE 2\n#endif\n#define GONE 1\n#undef GONE\nint p = PAGE + GONE;\n#define LATER 1\n";
        let cx = Context {
            include_dirs: vec![PathBuf::from("/p/include")],
            ..Default::default()
        };
        let line = 9;
        let col = src.lines().nth(line).unwrap().find("PAGE").unwrap();
        let e = expand_at(Path::new("/p/src/a.c"), src, line, col, &cx, &read).unwrap();
        assert_eq!(e.expanded, "((4) * 1024)");
        assert_eq!(e.defined, Some((PathBuf::from("/p/include/more.h"), 1)));
        assert_eq!(e.definition, "#define PAGE KB(4)");
        let col = src.lines().nth(line).unwrap().find("GONE").unwrap();
        // Undefined there: the line is expanded, GONE left as it is.
        let e = expand_at(Path::new("/p/src/a.c"), src, line, col, &cx, &read).unwrap();
        assert_eq!(
            (e.name.as_str(), e.expanded.as_str()),
            ("", "int p = ((4) * 1024) + GONE;")
        );
        // A define below the line is not in effect yet.
        let src2 = "int x = LATER;\n#define LATER 1\n";
        assert!(expand_at(Path::new("/p/src/a.c"), src2, 0, 8, &cx, &read).is_none());
    }

    #[test]
    fn command_line_macros_and_compile_commands() {
        let read = files(&[
            (
                "/p/build/compile_commands.json",
                r#"[{"directory": "/p/build", "file": "../src/a.c", "command": "cc -I../vendor -DLEVEL=3 -D FAST -UFAST -c ../src/a.c"},
                    {"directory": "/p", "file": "src/b.c", "arguments": ["cc", "-isystem", "third", "-c", "src/b.c"]}]"#,
            ),
            ("/p/vendor/v.h", "#define VENDOR \"v\"\n"),
        ]);
        let cx = Context::for_file(Path::new("/p"), Path::new("/p/src/a.c"), &read);
        assert_eq!(cx.include_dirs[0], PathBuf::from("/p/build/../vendor"));
        assert_eq!(cx.predefined, [("LEVEL".to_string(), "3".to_string())]);
        let src = "#include <v.h>\nint l = LEVEL; char *v = VENDOR;\n";
        let e = expand_at(Path::new("/p/src/a.c"), src, 1, 8, &cx, &read).unwrap();
        assert_eq!((e.expanded.as_str(), e.defined.clone()), ("3", None));
        let e = expand_at(
            Path::new("/p/src/a.c"),
            src,
            1,
            src.lines().nth(1).unwrap().find("VENDOR").unwrap(),
            &cx,
            &read,
        )
        .unwrap();
        assert_eq!(e.expanded, "\"v\"");
        assert_eq!(e.defined, Some((PathBuf::from("/p/vendor/v.h"), 1)));
        let b = Context::for_file(Path::new("/p"), Path::new("/p/src/b.c"), &read);
        assert_eq!(b.include_dirs[0], PathBuf::from("/p/third"));
        assert_eq!(
            split_command(r#"cc -DMSG="a b" 'x y' a\ b"#),
            ["cc", "-DMSG=a b", "x y", "a b"]
        );
    }

    #[test]
    fn runaways_are_cut_short() {
        // Each step doubles: 2^40 tokens if it were let run.
        let mut src = String::from("#define A0 x x\n");
        for k in 1..=40 {
            src.push_str(&format!("#define A{k} A{} A{}\n", k - 1, k - 1));
        }
        src.push_str("A40\n");
        let e = at(&src, 41, "A40").unwrap();
        assert!(e.truncated);
    }

    #[test]
    fn builtins_and_rendering() {
        let src = "#define WHERE __FILE__ \":\" __LINE__\n#define INC(x) x+++1\nWHERE;\nINC(i);\n";
        assert_eq!(expanded(src, 2, "WHERE"), "\"a.c\" \":\" 3");
        assert_eq!(expanded(src, 3, "INC"), "i+++1");
        let toks = vec![
            Tok::new("a", false),
            Tok::new("+", false),
            Tok::new("+", false),
            Tok::new("b", false),
        ];
        assert_eq!(render(&toks), "a+ +b");
    }

    #[test]
    fn in_a_project_on_disk_and_never_outside_it() {
        let dir = crate::testutil::TempDir::new("cmacro");
        let root = dir.path();
        std::fs::create_dir_all(root.join("include")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("include/util.h"), "#define TWICE(x) ((x) * 2)\n").unwrap();
        let outside = crate::testutil::TempDir::new("cmacro-outside");
        std::fs::write(outside.path().join("secret.h"), "#define SECRET 42\n").unwrap();
        let src = format!(
            "#include <util.h>\n#include \"{}\"\nint a = TWICE(3) + SECRET;\n",
            outside.path().join("secret.h").display()
        );
        std::fs::write(root.join("src/a.c"), src).unwrap();
        let host = Target::host();
        let e = expand_in_project(root, "src/a.c", 2, 8, &host)
            .unwrap()
            .unwrap();
        assert_eq!(e.expanded, "((3) * 2)");
        assert_eq!((e.file.as_str(), e.line), ("include/util.h", 1));
        // A header outside the project is not read: SECRET stays as written.
        let e = expand_in_project(root, "src/a.c", 2, 0, &host)
            .unwrap()
            .unwrap();
        assert_eq!(e.expanded, "int a = ((3) * 2) + SECRET;");
        assert!(expand_in_project(root, "../x.c", 0, 0, &host).is_err());
    }

    /// The expansions agree with the system's C preprocessor, token for
    /// token (spacing aside), over the standard's examples and the GNU
    /// extensions. Ignored by default: it runs `cpp`.
    #[test]
    #[ignore = "runs the system's cpp; run explicitly"]
    fn matches_the_c_preprocessor() {
        // Each group of uses with its own defines, as the standard's
        // examples are separate translation units.
        let example3 = "#define x 3\n#define f(a) f(x * (a))\n#undef x\n#define x 2\n#define g f\n#define z z[0]\n#define h g(~\n#define m(a) a(w)\n#define w 0,1\n#define t(a) a\n#define p() int\n#define q(x) x\n#define r(x,y) x ## y\n#define str(x) # x\n";
        let example4 = "#define str(s) # s\n#define xstr(s) str(s)\n#define debug(s, t) printf(\"x\" # s \"= %d, x\" # t \"= %s\", x ## s, x ## t)\n#define INCFILE(n) vers ## n\n#define glue(a, b) a ## b\n#define xglue(a, b) glue(a, b)\n#define HIGHLOW \"hello\"\n#define LOW LOW \", world\"\n";
        let gnu = "#define LOG(fmt, ...) printf(fmt, ## __VA_ARGS__)\n#define F(...) f2(0 __VA_OPT__(,) __VA_ARGS__)\n#define E(args...) e(args)\n#define CAT(a, b) a ## b\n#define INC(v) v+++1\n#define OBJ FN\n#define FN(a) [a]\n";
        let cases = [
            (example3, "f(y+1) + f(f(z)) % t(t(g)(0) + t)(1);"),
            (example3, "g(x+(3,4)-w) | h 5) & m(f)^m(m);"),
            (
                example3,
                "p() i[q()] = { q(1), r(2,3), r(4,), r(,5), r(,) };",
            ),
            (example3, "char c[2][6] = { str(hello), str() };"),
            (example4, "debug(1, 2);"),
            (example4, "glue(HIGH, LOW); xglue(HIGH, LOW)"),
            (example4, "xstr(INCFILE(2).h)"),
            (example4, "str(strncmp(\"abc\\0d\", \"abc\", '\\4') == 0)"),
            (gnu, "LOG(\"x\"); LOG(\"x %d %d\", 1, 2);"),
            (gnu, "F(); F(a, b); E(1, 2, 3); CAT(, y); CAT(x, );"),
            (gnu, "INC(i); OBJ(7);"),
        ];
        let dir = crate::testutil::TempDir::new("cmacro-cpp");
        let tokens = |s: &str| lex(s).into_iter().map(|t| t.text).collect::<Vec<_>>();
        for (defines, line) in cases {
            let src = format!("{defines}{line}\n");
            let file = dir.path().join("t.c");
            std::fs::write(&file, &src).unwrap();
            let out = std::process::Command::new("cpp")
                .args(["-P", "-std=gnu2x"])
                .arg(&file)
                .output()
                .expect("cpp runs");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let theirs = String::from_utf8_lossy(&out.stdout).to_string();
            let n = src.lines().count() - 1;
            let ours = expand_at(&file, &src, n, line.len(), &Context::default(), &|_| None)
                .map(|e| e.expanded)
                .unwrap_or_else(|| line.to_string());
            assert_eq!(tokens(&ours), tokens(theirs.trim()), "{line}");
        }
    }
}
