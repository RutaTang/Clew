//! What the code built into a plugin may read while it is compiled.
//!
//! The compiler reads files and environment variables when code asks it to:
//! `include_str!`, `include_bytes!`, `include!`, `env!`, `option_env!`, a
//! module's `#[path]`, `#![debugger_visualizer]` and `#[link]`. A plugin is
//! built from code an agent wrote, with crates from crates.io, and what the
//! compiler reads can come back: in a compiler message, in what
//! `plugin_try` shows, inside the plugin. The user's keys, say. So before
//! cargo compiles anything:
//!
//! - the plugin's own code ([`own_code`]) uses none of them. A plugin reads
//!   the project through `project`, and has no environment;
//! - a dependency ([`dependency`]) reads its own package's files only: an
//!   include names a file in the package (or one its build script wrote, in
//!   `OUT_DIR`), so does a `#[path]`, no file in it links out of it, and its
//!   library is in it. An include from a variable the build is not given
//!   cannot compile, so cannot read anything; one from a variable it is
//!   given (`HOME`) is refused. `env!` alone is left to the build's
//!   environment, which holds nothing of the user's (`crate::build`). A
//!   `#![debugger_visualizer]` file only goes into the dependency's own
//!   metadata, never into the plugin or a message, so only an absolute path
//!   there is refused.
//!
//! It reads Rust's tokens, so comments and strings are not taken for code
//! and the layout of the code does not matter. A use it cannot follow (a
//! macro passed `include_str` to call) is refused rather than guessed at.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// The macros that read a file.
const FILE_MACROS: &[&str] = &["include", "include_str", "include_bytes"];
/// The macros that read the environment.
const ENV_MACROS: &[&str] = &["env", "option_env"];

/// The most of a dependency's source read to check it, and of one file.
const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

// -- tokens -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    /// A string literal's text; `None` for a byte or C string.
    Str(Option<String>),
    /// Any other literal: a number, a character, a lifetime.
    Lit,
    Punct(char),
    Open(char),
    Close(char),
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    /// From 1.
    line: usize,
}

/// `text`'s tokens, as Rust reads them, comments left out.
fn lex(text: &str) -> Vec<Token> {
    let s: Vec<char> = text.chars().collect();
    let at = |i: usize| s.get(i).copied().unwrap_or('\0');
    let ident_start = |c: char| c == '_' || c.is_alphabetic();
    let ident_char = |c: char| c == '_' || c.is_alphanumeric();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 1;
    while i < s.len() {
        let c = s[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && at(i + 1) == '/' {
            while i < s.len() && s[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && at(i + 1) == '*' {
            let mut depth = 0usize;
            while i < s.len() {
                if s[i] == '/' && at(i + 1) == '*' {
                    depth += 1;
                    i += 2;
                } else if s[i] == '*' && at(i + 1) == '/' {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if s[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            continue;
        }
        let start = line;
        let mut push = |tok: Tok| out.push(Token { tok, line: start });
        // Raw strings: r"…", r#"…"#, and their b and c kinds.
        let prefix = match (c, at(i + 1)) {
            ('r', _) => Some((1, true)),
            ('b' | 'c', 'r') => Some((2, false)),
            _ => None,
        };
        if let Some((len, plain)) = prefix {
            let mut j = i + len;
            let mut hashes = 0;
            while at(j) == '#' {
                hashes += 1;
                j += 1;
            }
            if at(j) == '"' {
                j += 1;
                let begin = j;
                loop {
                    if j >= s.len() {
                        break;
                    }
                    if s[j] == '"' && (1..=hashes).all(|k| at(j + k) == '#') {
                        break;
                    }
                    if s[j] == '\n' {
                        line += 1;
                    }
                    j += 1;
                }
                let value: String = s[begin..j.min(s.len())].iter().collect();
                push(Tok::Str(plain.then_some(value)));
                i = j + 1 + hashes;
                continue;
            }
            // A raw identifier: r#name.
            if c == 'r' && at(i + 1) == '#' && ident_start(at(i + 2)) {
                let mut j = i + 2;
                while j < s.len() && ident_char(s[j]) {
                    j += 1;
                }
                push(Tok::Ident(s[i + 2..j].iter().collect()));
                i = j;
                continue;
            }
        }
        // Strings: "…", b"…", c"…".
        let quoted = match (c, at(i + 1)) {
            ('"', _) => Some((1, true)),
            ('b' | 'c', '"') => Some((2, false)),
            _ => None,
        };
        if let Some((len, plain)) = quoted {
            let mut j = i + len;
            let mut value = String::new();
            while j < s.len() && s[j] != '"' {
                if s[j] == '\\' {
                    j += 1;
                    match at(j) {
                        'n' => value.push('\n'),
                        't' => value.push('\t'),
                        'r' => value.push('\r'),
                        '0' => value.push('\0'),
                        'x' => {
                            let hex: String = s[j + 1..(j + 3).min(s.len())].iter().collect();
                            if let Ok(n) = u8::from_str_radix(&hex, 16) {
                                value.push(n as char);
                            }
                            j += 2;
                        }
                        'u' => {
                            let mut k = j + 1;
                            while k < s.len() && s[k] != '}' {
                                k += 1;
                            }
                            let hex: String = s[(j + 2).min(k)..k].iter().collect();
                            if let Some(ch) =
                                u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                            {
                                value.push(ch);
                            }
                            j = k;
                        }
                        '\n' => {
                            // A line continued: the next line's leading
                            // whitespace is not in the string.
                            line += 1;
                            while at(j + 1).is_whitespace() {
                                j += 1;
                                if s[j] == '\n' {
                                    line += 1;
                                }
                            }
                        }
                        other => value.push(other),
                    }
                    j += 1;
                    continue;
                }
                if s[j] == '\n' {
                    line += 1;
                }
                value.push(s[j]);
                j += 1;
            }
            push(Tok::Str(plain.then_some(value)));
            i = j + 1;
            continue;
        }
        // A character, a byte, or a lifetime.
        if c == '\'' || (c == 'b' && at(i + 1) == '\'') {
            let q = if c == 'b' { i + 1 } else { i };
            if at(q + 1) == '\\' {
                let mut j = q + 3;
                while j < s.len() && s[j] != '\'' && s[j] != '\n' {
                    j += 1;
                }
                push(Tok::Lit);
                i = j + 1;
                continue;
            }
            if at(q + 2) == '\'' {
                push(Tok::Lit);
                i = q + 3;
                continue;
            }
            // A lifetime or a label.
            let mut j = q + 1;
            while j < s.len() && ident_char(s[j]) {
                j += 1;
            }
            push(Tok::Lit);
            i = j.max(q + 1);
            continue;
        }
        if ident_start(c) {
            let mut j = i;
            while j < s.len() && ident_char(s[j]) {
                j += 1;
            }
            push(Tok::Ident(s[i..j].iter().collect()));
            i = j;
            continue;
        }
        if c.is_ascii_digit() {
            let hex = c == '0' && matches!(at(i + 1), 'x' | 'X');
            let mut dot = false;
            let mut j = i + 1;
            loop {
                let d = at(j);
                if d.is_alphanumeric() || d == '_' {
                    j += 1;
                } else if d == '.' && !dot && at(j + 1).is_ascii_digit() {
                    dot = true;
                    j += 1;
                } else if matches!(d, '+' | '-')
                    && !hex
                    && matches!(s[j - 1], 'e' | 'E')
                    && at(j + 1).is_ascii_digit()
                {
                    j += 1;
                } else {
                    break;
                }
            }
            push(Tok::Lit);
            i = j;
            continue;
        }
        match c {
            '(' | '[' | '{' => push(Tok::Open(c)),
            ')' | ']' | '}' => push(Tok::Close(c)),
            _ => push(Tok::Punct(c)),
        }
        i += 1;
    }
    out
}

/// Where each token is: in an attribute, in a macro's input, in a
/// `macro_rules!` definition (whose code runs wherever the macro is used).
#[derive(Debug, Clone, Copy, Default)]
struct Place {
    attribute: bool,
    macro_input: bool,
    rules: bool,
}

fn places(toks: &[Token]) -> Vec<Place> {
    #[derive(Clone, Copy, PartialEq)]
    enum Group {
        Plain,
        Attribute,
        Macro,
        Rules,
    }
    let mut stack: Vec<Group> = Vec::new();
    let mut out = Vec::with_capacity(toks.len());
    let tok = |i: usize| toks.get(i).map(|t| &t.tok);
    for (i, t) in toks.iter().enumerate() {
        match &t.tok {
            Tok::Open(c) => {
                let back = |n: usize| i.checked_sub(n).and_then(tok);
                // #[…] and #![…].
                let attribute = *c == '['
                    && (back(1) == Some(&Tok::Punct('#'))
                        || (back(1) == Some(&Tok::Punct('!'))
                            && back(2) == Some(&Tok::Punct('#'))));
                let group = if attribute {
                    Group::Attribute
                } else if matches!(back(1), Some(Tok::Ident(_)))
                    && back(2) == Some(&Tok::Punct('!'))
                    && back(3) == Some(&Tok::Ident("macro_rules".into()))
                {
                    Group::Rules
                } else if back(1) == Some(&Tok::Punct('!'))
                    && matches!(back(2), Some(Tok::Ident(_)))
                {
                    Group::Macro
                } else {
                    Group::Plain
                };
                stack.push(group);
                out.push(place(&stack));
            }
            Tok::Close(_) => {
                out.push(place(&stack));
                stack.pop();
            }
            _ => out.push(place(&stack)),
        }
    }
    fn place(stack: &[Group]) -> Place {
        Place {
            attribute: stack.contains(&Group::Attribute),
            macro_input: stack
                .iter()
                .any(|g| matches!(g, Group::Macro | Group::Rules)),
            rules: stack.contains(&Group::Rules),
        }
    }
    out
}

/// The tokens inside the group opening at `open`, and the index after it.
fn group(toks: &[Token], open: usize) -> Option<(&[Token], usize)> {
    if !matches!(toks.get(open)?.tok, Tok::Open(_)) {
        return None;
    }
    let mut depth = 0usize;
    for (k, t) in toks.iter().enumerate().skip(open) {
        match t.tok {
            Tok::Open(_) => depth += 1,
            Tok::Close(_) => {
                depth -= 1;
                if depth == 0 {
                    return Some((&toks[open + 1..k], k + 1));
                }
            }
            _ => {}
        }
    }
    None
}

fn is(toks: &[Token], i: usize, want: &Tok) -> bool {
    toks.get(i).is_some_and(|t| &t.tok == want)
}

/// Whether the name at `i` is a path's last part: `std::include_str`.
fn after_path(toks: &[Token], i: usize) -> bool {
    i >= 2 && is(toks, i - 1, &Tok::Punct(':')) && is(toks, i - 2, &Tok::Punct(':'))
}

// -- the plugin's own code ------------------------------------------------------

/// What in `text`, the plugin's file `rel`, would read the building
/// computer's files or environment: one sentence per place, for the agent.
pub(crate) fn own_code(rel: &str, text: &str) -> Vec<String> {
    let toks = lex(text);
    let places = places(&toks);
    let mut found = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let Tok::Ident(name) = &t.tok else { continue };
        let at = format!("{rel}:{}", t.line);
        let file = FILE_MACROS.contains(&name.as_str());
        let env = ENV_MACROS.contains(&name.as_str());
        if file || env {
            // A method or a field of that name is not the macro.
            if i > 0 && is(&toks, i - 1, &Tok::Punct('.')) {
                continue;
            }
            // The macro: called, renamed, named by its path, or handed to
            // a macro that could call it.
            let the_macro = is(&toks, i + 1, &Tok::Punct('!'))
                || is(&toks, i + 1, &Tok::Ident("as".into()))
                || after_path(&toks, i)
                || places[i].macro_input
                || places[i].attribute;
            if !the_macro {
                continue;
            }
            found.push(if file {
                format!(
                    "{at}: `{name}` reads a file of the computer the plugin is built on, which a plugin may not do. Read the project's files with project::read, or write what you need in the code."
                )
            } else {
                format!(
                    "{at}: `{name}` reads the environment of the computer the plugin is built on, which a plugin may not do: a plugin has no environment."
                )
            });
            continue;
        }
        if places[i].attribute
            && matches!(name.as_str(), "path" | "debugger_visualizer" | "link")
            && (is(&toks, i + 1, &Tok::Punct('=')) || is(&toks, i + 1, &Tok::Open('(')))
        {
            found.push(match name.as_str() {
                "path" => format!(
                    "{at}: #[path] can name a file outside the plugin's src folder: put the module's file in src, named as the module, and leave #[path] out."
                ),
                "link" => format!("{at}: #[link] links a library, which a plugin may not do."),
                _ => format!(
                    "{at}: #[debugger_visualizer] reads a file while building, which a plugin may not do: leave it out."
                ),
            });
        }
    }
    found
}

// -- a dependency ---------------------------------------------------------------

/// Check the package in `root` whose library is `lib`: everything the
/// compiler would read for it is in it. The error says what is not.
pub(crate) fn dependency(root: &Path, lib: &Path) -> Result<(), String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("its folder could not be read ({e})"))?;
    let lib = lib
        .canonicalize()
        .map_err(|e| format!("its library could not be read ({e})"))?;
    if !lib.starts_with(&root) {
        return Err(format!(
            "its library, {}, is outside the package",
            lib.display()
        ));
    }
    let mut check = Check {
        root: root.clone(),
        files: Vec::new(),
        seen: HashSet::new(),
        dirs: HashSet::new(),
        read: 0,
    };
    // Every Rust file beside the library and under it: what its modules
    // are; the files an include or a #[path] names join as they are found.
    let start = lib.parent().unwrap_or(&root).to_path_buf();
    check.walk(&start)?;
    check.add(lib);
    while let Some(file) = check.files.pop() {
        check.file(&file)?;
    }
    Ok(())
}

struct Check {
    root: PathBuf,
    /// Files still to read.
    files: Vec<PathBuf>,
    seen: HashSet<PathBuf>,
    dirs: HashSet<PathBuf>,
    read: u64,
}

impl Check {
    fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn add(&mut self, file: PathBuf) {
        if self.seen.insert(file.clone()) {
            self.files.push(file);
        }
    }

    /// The Rust files in `dir` and under it. A link is followed only to a
    /// place in the package.
    fn walk(&mut self, dir: &Path) -> Result<(), String> {
        if !self.dirs.insert(dir.to_path_buf()) {
            return Ok(());
        }
        let entries = std::fs::read_dir(dir)
            .map_err(|e| format!("{} could not be read ({e})", self.rel(dir)))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let path = if meta.file_type().is_symlink() {
                match path.canonicalize() {
                    Ok(target) if target.starts_with(&self.root) => target,
                    // A folder or a Rust file out of the package could be a
                    // module; any other file is read only if included,
                    // which is checked where it is.
                    Ok(target)
                        if !target.is_dir() && path.extension().is_none_or(|e| e != "rs") =>
                    {
                        continue;
                    }
                    _ => {
                        return Err(format!(
                            "{} links to a file outside the package",
                            self.rel(&path)
                        ));
                    }
                }
            } else {
                path
            };
            if path.is_dir() {
                self.walk(&path)?;
            } else if path.extension().is_some_and(|e| e == "rs") {
                self.add(path);
            }
        }
        Ok(())
    }

    /// `literal`, a path the compiler reads from `dir`, if it stays in the
    /// package: the file it names. `climb` is how far up it may go, when
    /// where it is read from is not `dir` but wherever a macro is used.
    fn inside(&self, dir: &Path, literal: &str, climb: Option<usize>) -> Result<PathBuf, String> {
        if literal.is_empty() || absolute(literal) {
            return Err(format!("\"{literal}\" is not a path in the package"));
        }
        let mut at: Vec<String> = dir
            .strip_prefix(&self.root)
            .unwrap_or(Path::new(""))
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        let start = at.len() as isize;
        let mut lowest = start;
        for part in literal.split(['/', '\\']) {
            match part {
                "" | "." => {}
                ".." => {
                    if at.pop().is_none() {
                        return Err(format!("\"{literal}\" names a file outside the package"));
                    }
                    lowest = lowest.min(at.len() as isize);
                }
                part => at.push(part.to_string()),
            }
        }
        if let Some(climb) = climb
            && start - lowest > climb as isize
        {
            return Err(format!(
                "\"{literal}\", in a macro, could name a file outside the package"
            ));
        }
        let path = at.iter().fold(self.root.clone(), |p, part| p.join(part));
        match path.canonicalize() {
            Ok(real) if !real.starts_with(&self.root) => {
                Err(format!("\"{literal}\" names a file outside the package"))
            }
            Ok(real) => Ok(real),
            // Not there: the compiler will say so.
            Err(_) => Ok(path),
        }
    }

    fn file(&mut self, path: &Path) -> Result<(), String> {
        let rel = self.rel(path);
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        self.read += size;
        if size > MAX_FILE_BYTES || self.read > MAX_PACKAGE_BYTES {
            return Err("its source is too large to check".into());
        }
        let Ok(bytes) = std::fs::read(path) else {
            // Not there, or not readable: the compiler will say so.
            return Ok(());
        };
        let text = String::from_utf8_lossy(&bytes);
        let toks = lex(&text);
        let places = places(&toks);
        let dir = path.parent().unwrap_or(&self.root).to_path_buf();
        for (i, t) in toks.iter().enumerate() {
            let Tok::Ident(name) = &t.tok else { continue };
            let at = format!("{rel}:{}", t.line);
            if FILE_MACROS.contains(&name.as_str()) {
                if i > 0 && is(&toks, i - 1, &Tok::Punct('.')) {
                    continue;
                }
                if is(&toks, i + 1, &Tok::Punct('!')) {
                    let Some((input, _)) = group(&toks, i + 2) else {
                        return Err(format!("{at}: `{name}!` is not called as Clew can read"));
                    };
                    self.include(name, input, &dir, places[i].rules)
                        .map_err(|why| format!("{at}: {name}!: {why}"))?;
                    continue;
                }
                // `include` is a common name for other things; the macro
                // is only reached through it renamed, named by its path,
                // or handed to a macro that could call it. The other two
                // names are only ever the macros.
                let reached = name != "include"
                    || is(&toks, i + 1, &Tok::Ident("as".into()))
                    || after_path(&toks, i)
                    || places[i].macro_input
                    || places[i].attribute;
                if reached {
                    return Err(format!(
                        "{at}: `{name}` is used in a way Clew cannot follow, to check what it reads"
                    ));
                }
                continue;
            }
            if !places[i].attribute {
                continue;
            }
            match name.as_str() {
                // #[path = "…"], and cfg_attr(…, path = "…").
                "path"
                    if is(&toks, i + 1, &Tok::Punct('='))
                        && i > 0
                        && matches!(toks[i - 1].tok, Tok::Open('[') | Tok::Punct(',')) =>
                {
                    let Some(Tok::Str(Some(literal))) = toks.get(i + 2).map(|t| &t.tok) else {
                        return Err(format!("{at}: #[path] is not a plain path"));
                    };
                    let file = self
                        .inside(&dir, literal, None)
                        .map_err(|why| format!("{at}: #[path]: {why}"))?;
                    self.add(file);
                }
                "debugger_visualizer" if is(&toks, i + 1, &Tok::Open('(')) => {
                    let (input, _) = group(&toks, i + 1).unwrap_or((&[], 0));
                    for t in input {
                        match &t.tok {
                            Tok::Str(Some(literal)) if absolute(literal) => {
                                return Err(format!(
                                    "{at}: #[debugger_visualizer]: \"{literal}\" is not a path in the package"
                                ));
                            }
                            Tok::Str(None) => {
                                return Err(format!("{at}: #[debugger_visualizer] is not plain"));
                            }
                            _ => {}
                        }
                    }
                }
                "link" if is(&toks, i + 1, &Tok::Open('(')) => {
                    let (input, _) = group(&toks, i + 1).unwrap_or((&[], 0));
                    for (k, t) in input.iter().enumerate() {
                        let Tok::Ident(key) = &t.tok else { continue };
                        if !is(input, k + 1, &Tok::Punct('=')) {
                            continue;
                        }
                        let value = match input.get(k + 2).map(|t| &t.tok) {
                            Some(Tok::Str(Some(v))) => v.as_str(),
                            _ => return Err(format!("{at}: #[link] is not plain")),
                        };
                        let bad = match key.as_str() {
                            "name" => value.contains(['/', '\\']) || value.contains(".."),
                            "modifiers" => value.contains("verbatim"),
                            _ => false,
                        };
                        if bad {
                            return Err(format!(
                                "{at}: #[link({key} = \"{value}\")] could name a file outside the package"
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// An include's input: a path in the package, or one under the build's
    /// own output (`OUT_DIR`). `rules`: it is in a `macro_rules!`, so read
    /// from wherever the macro is used.
    fn include(
        &mut self,
        name: &str,
        input: &[Token],
        dir: &Path,
        rules: bool,
    ) -> Result<(), String> {
        let mut input = input;
        if let Some(last) = input.last()
            && last.tok == Tok::Punct(',')
        {
            input = &input[..input.len() - 1];
        }
        let code = name == "include";
        let plain = || "its path is not one Clew can check".to_string();
        let path = match parse_path(input).ok_or_else(plain)? {
            Named::Literal(literal) => {
                // Code included from wherever a macro is used: Clew cannot
                // tell which file that is, to check it too.
                if rules && code {
                    return Err(
                        "it is in a macro, so Clew cannot tell which file it includes".into(),
                    );
                }
                self.inside(dir, &literal, rules.then_some(1))?
            }
            Named::Manifest(literal) => {
                if rules && code {
                    return Err(
                        "it is in a macro, so Clew cannot tell which file it includes".into(),
                    );
                }
                let rest = literal.trim_start_matches(['/', '\\']);
                self.inside(&self.root.clone(), rest, Some(0))?
            }
            // What a build script wrote (only the few allowed may run), in
            // its own folder.
            Named::OutDir(rest) => {
                if rest.split(['/', '\\']).any(|part| part == "..") {
                    return Err(format!("\"{rest}\" leaves the build's own folder"));
                }
                return Ok(());
            }
            // A variable the build is given names a folder of the
            // computer's; one it is not given fails to compile.
            Named::Variable(var) => {
                if given(&var) {
                    return Err(format!("it reads from {var}, a folder of the computer's"));
                }
                return Ok(());
            }
        };
        if code {
            self.add(path);
        }
        Ok(())
    }
}

/// What an include reads.
enum Named {
    /// A path, from the file it is in.
    Literal(String),
    /// `concat!(env!("CARGO_MANIFEST_DIR"), …)`: a path from the package.
    Manifest(String),
    /// `concat!(env!("OUT_DIR"), …)`: what its build script wrote.
    OutDir(String),
    /// `concat!(env!(VAR), …)` of another variable.
    Variable(String),
}

/// Whether `literal` is a path from the root of a drive.
fn absolute(literal: &str) -> bool {
    literal.starts_with(['/', '\\'])
        || literal.as_bytes().get(1) == Some(&b':')
        || Path::new(literal).is_absolute()
}

/// Whether the compiler, building a plugin, could see the variable `name`:
/// one the build is given ([`crate::build::kept`]), or one cargo sets.
fn given(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    crate::build::kept(name, true)
        || upper.starts_with("CARGO")
        || upper.starts_with("RUST")
        || upper == "OUT_DIR"
}

/// `"path"`, or `concat!` of an `env!("CARGO_MANIFEST_DIR")` or
/// `env!("OUT_DIR")` and strings. Anything else is `None`.
fn parse_path(input: &[Token]) -> Option<Named> {
    if let [
        Token {
            tok: Tok::Str(Some(s)),
            ..
        },
    ] = input
    {
        return Some(Named::Literal(s.clone()));
    }
    let input = macro_path(input, "concat")?;
    let (inner, end) = group(input, 0)?;
    if end != input.len() {
        return None;
    }
    let mut parts = inner
        .split(|t| t.tok == Tok::Punct(','))
        .filter(|p| !p.is_empty());
    let first = parts.next()?;
    let mut text = String::new();
    let base = match first {
        [
            Token {
                tok: Tok::Str(Some(s)),
                ..
            },
        ] => {
            text.push_str(s);
            None
        }
        _ => {
            let env = macro_path(first, "env")?;
            let (var, end) = group(env, 0)?;
            let var = match (var, end == env.len()) {
                (
                    [
                        Token {
                            tok: Tok::Str(Some(v)),
                            ..
                        },
                    ],
                    true,
                )
                | (
                    [
                        Token {
                            tok: Tok::Str(Some(v)),
                            ..
                        },
                        Token {
                            tok: Tok::Punct(','),
                            ..
                        },
                    ],
                    true,
                ) => v.clone(),
                _ => return None,
            };
            Some(var)
        }
    };
    for part in parts {
        match part {
            [
                Token {
                    tok: Tok::Str(Some(s)),
                    ..
                },
            ] => text.push_str(s),
            _ => return None,
        }
    }
    match base {
        None => Some(Named::Literal(text)),
        Some(var) if var == "CARGO_MANIFEST_DIR" => Some(Named::Manifest(text)),
        Some(var) if var == "OUT_DIR" => Some(Named::OutDir(text)),
        Some(var) => Some(Named::Variable(var)),
    }
}

/// `name!(…)`, `core::name!(…)`, `std::name!(…)` or `::core::name!(…)`:
/// the tokens from its group on.
fn macro_path<'t>(toks: &'t [Token], name: &str) -> Option<&'t [Token]> {
    let mut i = 0;
    loop {
        match toks.get(i).map(|t| &t.tok) {
            Some(Tok::Punct(':')) if is(toks, i + 1, &Tok::Punct(':')) => i += 2,
            Some(Tok::Ident(n))
                if (n == "core" || n == "std") && is(toks, i + 1, &Tok::Punct(':')) =>
            {
                i += 1;
            }
            Some(Tok::Ident(n)) if n == name && is(toks, i + 1, &Tok::Punct('!')) => {
                return Some(&toks[i + 2..]);
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own(text: &str) -> Vec<String> {
        own_code("src/lib.rs", text)
    }

    #[test]
    fn the_lexer_reads_code_not_comments_or_strings() {
        let toks = lex(
            "// include_str!(\"x\")\n/* env!(\"A\") /* nested */ */ let s = \"include_str!\\\"\";\n\
             let r = r#\"a \" include!(\"b\")\"#; let c = '\"'; let l: &'a str = \"\\x41\\u{42}\";\n\
             let b = b\"bytes\"; fn r#match() {} let f = 1.5e-3; let t = x.0.1;",
        );
        let idents: Vec<&str> = toks
            .iter()
            .filter_map(|t| match &t.tok {
                Tok::Ident(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            !idents
                .iter()
                .any(|n| n.starts_with("include") || *n == "env")
        );
        assert!(idents.contains(&"match"));
        let strings: Vec<Option<String>> = toks
            .iter()
            .filter_map(|t| match &t.tok {
                Tok::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            strings,
            [
                Some("include_str!\"".into()),
                Some("a \" include!(\"b\")".into()),
                Some("AB".into()),
                None
            ]
        );
        assert_eq!(toks.last().unwrap().line, 4);
    }

    #[test]
    fn a_plugin_may_not_read_files_or_the_environment_while_built() {
        for refused in [
            "const K: &str = include_str!(\"../../../config.toml\");",
            "const K: &[u8] = include_bytes!(\"/dev/zero\");",
            "include!(\"other.rs\");",
            "const H: &str = env!(\"HOME\");",
            "const H: Option<&str> = option_env!(\"KEY\");",
            "const K: &str = std::include_str!(\"x\");",
            "const K: &str = core :: include_str ! (\"x\");",
            "use core::include_str as text; const K: &str = text!(\"x\");",
            "use std::{include_bytes as b};",
            "use std::env;",
            "macro_rules! call { ($m:ident) => { $m!(\"x\") } } const K: &str = call!(include_str);",
            "#[doc = include_str!(\"../README.md\")] pub struct A;",
            "#[path = \"../../../x.rs\"] mod x;",
            "#[cfg_attr(all(), path = \"/etc/x.rs\")] mod x;",
            "#![debugger_visualizer(gdb_script_file = \"/etc/passwd\")]",
            "#[link(name = \"x\")] extern \"C\" {}",
            "const K: &str = r#include_str!(\"x\");",
        ] {
            assert!(!own(refused).is_empty(), "{refused}");
        }
        for allowed in [
            "let hits = project::search(&Query::text(\"TODO\").include(\"src/**/*.rs\"));",
            "struct Q { include: Vec<String> } fn f(q: Q) -> usize { q.include.len() }",
            "let env = 3; let include = env + 1;",
            "// include_str!(\"/etc/passwd\")\nlet s = \"env!(\\\"HOME\\\")\";",
            "#[derive(Debug)] #[serde(rename = \"path\")] struct A;",
            "let path = \"src/a.rs\"; let p = project::read(path);",
        ] {
            assert!(own(allowed).is_empty(), "{allowed}: {:?}", own(allowed));
        }
        let found = own("fn a() {}\nconst K: &str = include_str!(\"k\");");
        assert_eq!(found.len(), 1);
        assert!(
            found[0].starts_with("src/lib.rs:2: `include_str` reads a file"),
            "{found:?}"
        );
    }

    /// A package laid out in a temporary folder.
    struct Package {
        root: PathBuf,
    }

    impl Package {
        fn new(name: &str, files: &[(&str, &str)]) -> Self {
            let root = std::env::temp_dir().join(format!(
                "clew-reads-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            for (rel, text) in files {
                let path = root.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            }
            Self { root }
        }

        fn check(&self) -> Result<(), String> {
            dependency(&self.root, &self.root.join("src/lib.rs"))
        }
    }

    impl Drop for Package {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_dependency_reads_only_its_own_package() {
        let ok = |lib: &str| {
            Package::new(
                "ok",
                &[
                    ("src/lib.rs", lib),
                    ("README.md", "hi"),
                    ("src/data.txt", "d"),
                ],
            )
            .check()
        };
        for allowed in [
            "#![doc = include_str!(\"../README.md\")]",
            "const D: &str = include_str!(\"data.txt\");",
            "const D: &str = include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/README.md\"));",
            "include!(concat!(env!(\"OUT_DIR\"), \"/private.rs\"));",
            "const V: &str = env!(\"CARGO_PKG_VERSION\");",
            "macro_rules! doc { () => { include_str!(\"../README.md\") } }",
            "fn include(&self) {} fn f(q: &Q) { q.include(); }",
            "#[link(wasm_import_module = \"clew:plugin/host\")] extern \"C\" {}",
            "#[cfg_attr(windows, link(name = \"kernel32\"))] extern \"C\" {}",
            // As the ICU4X data crates do: a variable the build is not
            // given cannot compile.
            "#[cfg(custom)] include!(concat!(core::env!(\"ICU4X_DATA_DIR\"), \"/mod.rs\"));",
            // As url does: the file only goes into its own metadata.
            "#![cfg_attr(feature = \"v\", debugger_visualizer(natvis_file = \"../../debug_metadata/url.natvis\"))]",
        ] {
            assert!(ok(allowed).is_ok(), "{allowed}: {:?}", ok(allowed));
        }
        for refused in [
            "const K: &str = include_str!(\"../../../../.cargo/credentials.toml\");",
            "const K: &str = include_str!(\"/home/u/.config/clew/config.toml\");",
            "const K: &str = include_str!(\"C:\\\\Users\\\\u\\\\key\");",
            "const K: &str = include_str!(concat!(env!(\"HOME\"), \"/.ssh/id_ed25519\"));",
            "const K: &str = include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../../x\"));",
            "macro_rules! inc { ($p:expr) => { include_str!($p) } }",
            "macro_rules! doc { () => { include_str!(\"../../x\") } }",
            "macro_rules! code { () => { include!(\"gen.rs\") } }",
            "macro_rules! call { ($m:ident) => { $m!(\"/etc/passwd\") } } call!(include);",
            "use core::include_str as text;",
            "pub use std::include;",
            "#[path = \"../../../x.rs\"] mod x;",
            "#[link(name = \"x\", kind = \"static\", modifiers = \"+verbatim\")] extern \"C\" {}",
            "#[link(name = \"../../../../etc/x\")] extern \"C\" {}",
            "#![debugger_visualizer(gdb_script_file = \"/etc/passwd\")]",
            "include!(concat!(env!(\"OUT_DIR\"), \"/../../../release/deps/x.rlib\"));",
            "const K: &str = include_str!(concat!(env!(\"USERPROFILE\"), \"/key\"));",
            "const K: &str = include_str!(concat!(env!(\"CARGO_HOME\"), \"/credentials.toml\"));",
            "const K: &str = include_str!(concat!(env!(\"RUSTUP_HOME\"), \"/x\"));",
        ] {
            assert!(ok(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn what_a_dependency_includes_is_checked_too() {
        let package = Package::new(
            "follows",
            &[
                ("src/lib.rs", "include!(\"../gen/tables.rs\");\nmod inner;"),
                (
                    "gen/tables.rs",
                    "const K: &str = include_str!(\"/etc/passwd\");",
                ),
                ("src/inner.rs", "pub fn f() {}"),
            ],
        );
        let why = package.check().unwrap_err();
        assert!(why.starts_with("gen/tables.rs:1: include_str!"), "{why}");

        let package = Package::new(
            "modules",
            &[
                ("src/lib.rs", "mod inner;"),
                ("src/inner/mod.rs", "mod deep;"),
                (
                    "src/inner/deep.rs",
                    "const K: &[u8] = include_bytes!(\"../../../../k\");",
                ),
            ],
        );
        assert!(
            package
                .check()
                .unwrap_err()
                .starts_with("src/inner/deep.rs:1:")
        );

        let package = Package::new(
            "path",
            &[
                ("src/lib.rs", "#[path = \"../other/x.rs\"] mod x;"),
                (
                    "other/x.rs",
                    "const H: &str = include_str!(\"../../../../h\");",
                ),
            ],
        );
        assert!(package.check().unwrap_err().starts_with("other/x.rs:1:"));
    }

    #[cfg(unix)]
    #[test]
    fn a_dependency_file_linking_out_of_its_package_is_refused() {
        let outside = Package::new("outside", &[("secret.rs", "pub const K: &str = \"k\";")]);
        let package = Package::new("links", &[("src/lib.rs", "mod secret;")]);
        std::os::unix::fs::symlink(
            outside.root.join("secret.rs"),
            package.root.join("src/secret.rs"),
        )
        .unwrap();
        let why = package.check().unwrap_err();
        assert!(
            why.contains("src/secret.rs links to a file outside"),
            "{why}"
        );

        let lib_outside = Package::new("lib-outside", &[("Cargo.toml", "")]);
        let why = dependency(&lib_outside.root, &outside.root.join("secret.rs")).unwrap_err();
        assert!(why.contains("outside the package"), "{why}");
    }
}
