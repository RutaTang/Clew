//! Value flow, read off the text: what a line does with an identifier —
//! declares or assigns it, takes it as a parameter, passes it to a call,
//! returns it, branches on it, reaches into it, or just reads it — and, for
//! a call, which function and which argument, so the trace can follow the
//! value into the callee's parameter. Where a value comes from and where it
//! goes, in the reader's terms, from the occurrences a language server
//! finds.
//!
//! Text, not a parse: one line at a time, in any language, with the small
//! set of spellings the supported languages share. A line the heuristics
//! misread is still shown — as a plain read, with its text — so nothing is
//! hidden, only less well named.

/// What a line does with the traced value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    /// Its declaration (the language server's definition).
    Declared,
    /// Assigned here: `x = …`, `let x = …`, `x += …`, `x := …`.
    Assigned,
    /// Bound as a parameter of a function or closure.
    Parameter,
    /// Passed as an argument to a call (`detail` names the callee).
    Passed,
    /// Returned from the enclosing function.
    Returned,
    /// Branched or looped on: `if`, `while`, `match`, `switch`, a comparison.
    Branched,
    /// Reached into: `x.field`, `x->field`, `x[i]`, `x.method()`.
    Member,
    /// Read otherwise.
    Read,
}

impl Role {
    pub const ALL: &'static [Role] = &[
        Role::Declared,
        Role::Assigned,
        Role::Parameter,
        Role::Passed,
        Role::Returned,
        Role::Branched,
        Role::Member,
        Role::Read,
    ];

    /// The section heading the FLOW tab files the role under.
    pub fn heading(self) -> &'static str {
        match self {
            Role::Declared => "DECLARED",
            Role::Assigned => "ASSIGNED",
            Role::Parameter => "PARAMETER",
            Role::Passed => "PASSED TO",
            Role::Returned => "RETURNED",
            Role::Branched => "BRANCHED ON",
            Role::Member => "MEMBER ACCESS",
            Role::Read => "READ",
        }
    }

    /// A short tag for a row.
    pub fn tag(self) -> &'static str {
        match self {
            Role::Declared => "decl",
            Role::Assigned => "set",
            Role::Parameter => "param",
            Role::Passed => "→",
            Role::Returned => "ret",
            Role::Branched => "if",
            Role::Member => ".",
            Role::Read => "read",
        }
    }

    /// Where the value comes from (true) or goes (false).
    pub fn is_source(self) -> bool {
        matches!(self, Role::Declared | Role::Assigned | Role::Parameter)
    }
}

/// A line's use of the traced value: the role and a detail — the callee for
/// a `Passed` (with the argument's index, 0-based), what was assigned for an
/// `Assigned` (a call's name, another identifier, or "a literal").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Use {
    pub role: Role,
    pub detail: String,
    /// For `Passed`: the argument's 0-based index in the call.
    pub argument: Option<usize>,
    /// For `Passed`: the callee's name and where it starts on the line (a
    /// char column), for a definition lookup.
    pub callee: Option<(String, usize)>,
}

const KEYWORDS_BEFORE_PAREN: &[&str] = &[
    "if", "while", "for", "match", "switch", "return", "elif", "catch", "except", "foreach",
    "until", "unless", "case", "with", "and", "or", "not", "in", "await", "yield", "let", "print",
];

const DECLARATION_WORDS: &[&str] = &[
    "let", "var", "const", "val", "mut", "auto", "static", "final", "def", "my", "local",
];

const FUNCTION_WORDS: &[&str] = &["fn", "def", "func", "function", "sub", "proc", "lambda"];

/// What `line` does with `word`, which starts at char column `col` of it.
pub fn classify(line: &str, col: usize, word: &str) -> Use {
    let chars: Vec<char> = line.chars().collect();
    let col = col.min(chars.len());
    let before: String = chars[..col].iter().collect();
    let after: String = chars[col.saturating_add(word.chars().count()).min(chars.len())..]
        .iter()
        .collect();
    let after_trim = after.trim_start();
    let before_trim = before.trim_end();
    let plain = |role: Role| Use {
        role,
        detail: String::new(),
        argument: None,
        callee: None,
    };

    // Reaching into the value.
    if after_trim.starts_with('.') && !after_trim.starts_with("..")
        || after_trim.starts_with("->")
        || after_trim.starts_with('[')
    {
        return plain(Role::Member);
    }
    // Assigned: `x = …` (not `==`, `=>`, `<=`, `>=`, `!=`), `x += …`, `x := …`,
    // a Go `x <- …` is a send, not an assignment.
    let assigned_by = |after_trim: &str| -> Option<&'static str> {
        let a = after_trim;
        if a.starts_with(":=") {
            return Some(":=");
        }
        for op in [
            "+=", "-=", "*=", "/=", "%=", "|=", "&=", "^=", "<<=", ">>=", "??=", "||=", "&&=",
        ] {
            if a.starts_with(op) {
                return Some(op);
            }
        }
        if a.starts_with('=') && !a.starts_with("==") && !a.starts_with("=>") {
            return Some("=");
        }
        None
    };
    // A type annotation between the name and its `=`: `x: T = …`, `x: Vec<T> = …`.
    let after_annotation = if after_trim.starts_with(':') && !after_trim.starts_with("::") {
        let mut depth = 0i32;
        let mut cut = None;
        for (i, c) in after_trim.char_indices().skip(1) {
            match c {
                '<' | '(' | '[' => depth += 1,
                '>' | ')' | ']' => depth -= 1,
                '=' if depth <= 0 => {
                    cut = Some(i);
                    break;
                }
                ',' | ';' | '{' if depth <= 0 => break,
                _ => {}
            }
        }
        cut.map(|i| &after_trim[i..])
    } else {
        None
    };
    if let Some(op) = assigned_by(after_trim).or_else(|| after_annotation.and_then(assigned_by)) {
        let rhs = after_trim
            .split_once(op)
            .map(|(_, r)| r)
            .or_else(|| after_annotation.and_then(|a| a.split_once(op).map(|(_, r)| r)))
            .unwrap_or("")
            .trim();
        return Use {
            role: Role::Assigned,
            detail: assigned_from(rhs),
            argument: None,
            callee: None,
        };
    }
    // Declared without a value on this line: `let x;`, `var x: T`.
    let last_word_before = before_trim
        .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|w| !w.is_empty());
    let declared_here = last_word_before.is_some_and(|w| DECLARATION_WORDS.contains(&w))
        || before_trim.ends_with("&mut")
        || before_trim.ends_with("mut");
    // A parameter: inside the parentheses of a function declaration.
    if let Some((callee, _index, _col)) = call_before(&chars, col)
        && FUNCTION_WORDS
            .iter()
            .any(|kw| before.split_whitespace().any(|w| w == *kw))
        && !KEYWORDS_BEFORE_PAREN.contains(&callee.as_str())
    {
        return plain(Role::Parameter);
    }
    if declared_here {
        return Use {
            role: Role::Assigned,
            detail: "declared".into(),
            argument: None,
            callee: None,
        };
    }
    // Returned.
    let first_word = line
        .trim_start()
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .unwrap_or("");
    if first_word == "return" || first_word == "yield" {
        return plain(Role::Returned);
    }
    // Passed to a call.
    if let Some((callee, index, callee_col)) = call_before(&chars, col) {
        if KEYWORDS_BEFORE_PAREN.contains(&callee.as_str()) {
            return plain(Role::Branched);
        }
        return Use {
            role: Role::Passed,
            detail: callee.clone(),
            argument: Some(index),
            callee: Some((callee, callee_col)),
        };
    }
    // Branched on.
    if matches!(
        first_word,
        "if" | "while" | "elif" | "match" | "switch" | "case" | "for" | "unless" | "until"
    ) || after_trim.starts_with("==")
        || after_trim.starts_with("!=")
        || after_trim.starts_with("<")
        || after_trim.starts_with(">")
        || after_trim.starts_with("&&")
        || after_trim.starts_with("||")
        || after_trim.starts_with('?')
        || before_trim.ends_with("==")
        || before_trim.ends_with("!=")
        || before_trim.ends_with("&&")
        || before_trim.ends_with("||")
    {
        return plain(Role::Branched);
    }
    plain(Role::Read)
}

/// What an assignment's right-hand side is: the called function, another
/// identifier, or a literal.
fn assigned_from(rhs: &str) -> String {
    let rhs = rhs.trim().trim_end_matches(';').trim();
    if rhs.is_empty() {
        return String::new();
    }
    let first = rhs
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == ':'))
        .find(|w| !w.is_empty())
        .unwrap_or("");
    if rhs
        .starts_with(|c: char| c.is_ascii_digit() || c == '"' || c == '\'' || c == '[' || c == '{')
        || matches!(first, "true" | "false" | "None" | "null" | "nil")
    {
        return "a literal".into();
    }
    if first.is_empty() {
        return String::new();
    }
    let rest = &rhs[rhs.find(first).map_or(0, |i| i + first.len())..];
    if rest.trim_start().starts_with('(') || matches!(first, "new" | "await") {
        let name = if matches!(first, "new" | "await") {
            rhs[first.len()..]
                .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == ':'))
                .find(|w| !w.is_empty())
                .unwrap_or(first)
        } else {
            first
        };
        format!("{}()", name.rsplit(['.', ':']).next().unwrap_or(name))
    } else {
        first.rsplit(['.', ':']).next().unwrap_or(first).to_string()
    }
}

/// The call the char column `col` sits inside the argument list of, on
/// `chars`: the callee's name, the 0-based index of the argument `col` is in,
/// and the callee's start column. Found by walking back to the unmatched
/// `(` and counting the commas at its depth. `None` outside any argument
/// list, or for a `(` no name precedes (a tuple, a grouping).
pub fn call_before(chars: &[char], col: usize) -> Option<(String, usize, usize)> {
    let mut depth = 0i32;
    let mut commas = 0usize;
    let mut i = col.min(chars.len());
    let mut open = None;
    while i > 0 {
        i -= 1;
        match chars[i] {
            ')' | ']' | '}' => depth += 1,
            '(' if depth == 0 => {
                open = Some(i);
                break;
            }
            '[' | '{' if depth == 0 => return None,
            '(' | '[' | '{' => depth -= 1,
            ',' if depth == 0 => commas += 1,
            _ => {}
        }
    }
    let open = open?;
    // The name before the `(`, over whitespace, generics and paths.
    let mut end = open;
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    if end > 0 && chars[end - 1] == '>' {
        // A turbofish or generic call: skip the `<…>`.
        let mut d = 0i32;
        while end > 0 {
            end -= 1;
            match chars[end] {
                '>' => d += 1,
                '<' => {
                    d -= 1;
                    if d == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        if end > 0 && chars[end - 1] == ':' && end > 1 && chars[end - 2] == ':' {
            end -= 2;
        }
    }
    let mut start = end;
    while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_') {
        start -= 1;
    }
    if start == end {
        return None;
    }
    let name: String = chars[start..end].iter().collect();
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some((name, commas, start))
}

/// The `index`-th (0-based) parameter of the declaration on `signature`: its
/// name and start column, for a language spelled `lang` ("go" names come
/// first, "java"/"c"/"cpp"/"dart" have the type first, the rest annotate
/// with `:` or have the name alone). `self`/`this`/`cls` are not counted.
pub fn parameter_at(signature: &str, index: usize, lang: &str) -> Option<(String, usize)> {
    let chars: Vec<char> = signature.chars().collect();
    let open = chars.iter().position(|&c| c == '(')?;
    let mut depth = 0i32;
    let mut parts: Vec<(usize, usize)> = Vec::new();
    let mut part_start = open + 1;
    let mut close = chars.len();
    for (i, &c) in chars.iter().enumerate().skip(open + 1) {
        match c {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' if depth == 0 => {
                close = i;
                break;
            }
            ')' | ']' | '}' | '>' => depth -= 1,
            ',' if depth == 0 => {
                parts.push((part_start, i));
                part_start = i + 1;
            }
            _ => {}
        }
    }
    parts.push((part_start, close));
    let name_in = |from: usize, to: usize| -> Option<(String, usize)> {
        let text: String = chars[from..to].iter().collect();
        let mut idents: Vec<(usize, usize)> = Vec::new();
        let mut at = None;
        for (i, c) in text.char_indices() {
            if c.is_alphanumeric() || c == '_' {
                if at.is_none() {
                    at = Some(i);
                }
            } else if let Some(s) = at.take() {
                idents.push((s, i));
            }
        }
        if let Some(s) = at {
            idents.push((s, text.len()));
        }
        let pick = if let Some(colon) = text.find(':').filter(|&c| !text[c..].starts_with("::")) {
            idents.iter().rev().find(|&&(_, e)| e <= colon).copied()
        } else if matches!(lang, "java" | "c" | "cpp" | "dart" | "kotlin_java") {
            idents.last().copied()
        } else if lang == "go" {
            idents.first().copied()
        } else {
            idents.iter().find(|&&(s, _)| !matches!(&text[s..], t if t.starts_with("mut ") || t.starts_with("ref "))).copied()
        }?;
        let name = text[pick.0..pick.1].to_string();
        if name.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        let col = chars[..from].len() + text[..pick.0].chars().count();
        Some((name, col))
    };
    let mut seen = 0usize;
    for &(from, to) in &parts {
        let Some((name, col)) = name_in(from, to) else {
            continue;
        };
        if matches!(name.as_str(), "self" | "this" | "cls") {
            continue;
        }
        if seen == index {
            return Some((name, col));
        }
        seen += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(line: &str, word: &str) -> Use {
        let col = line.find(word).expect("the word is on the line");
        classify(line, line[..col].chars().count(), word)
    }

    #[test]
    fn a_line_says_what_it_does_with_the_value() {
        assert_eq!(
            of("    let total = price * qty;", "total").role,
            Role::Assigned
        );
        assert_eq!(
            of("    let total = compute(price);", "total").detail,
            "compute()"
        );
        assert_eq!(of("    total = other;", "total").detail, "other");
        assert_eq!(of("    total += 1;", "total").role, Role::Assigned);
        assert_eq!(of("    x: Vec<u8> = Vec::new();", "x").detail, "new()");
        assert_eq!(of("    count := 0", "count").detail, "a literal");
        assert_eq!(of("    cfg = self.load()", "cfg").detail, "load()");
        assert_eq!(of("    let x;", "x").detail, "declared");
        assert_eq!(
            of("fn render(total: Money, dpi: u32) {", "total").role,
            Role::Parameter
        );
        assert_eq!(
            of("def render(self, total, dpi):", "dpi").role,
            Role::Parameter
        );
        assert_eq!(of("    return total;", "total").role, Role::Returned);
        let passed = of("    let s = format_money(total, locale);", "total");
        assert_eq!(passed.role, Role::Passed);
        assert_eq!(passed.detail, "format_money");
        assert_eq!(passed.argument, Some(0));
        assert_eq!(of("    emit(a, b, total)", "total").argument, Some(2));
        assert_eq!(of("    emit(a, (b, c), total)", "total").argument, Some(2));
        assert_eq!(
            of("    Vec::<u8>::with_capacity(total)", "total").detail,
            "with_capacity"
        );
        assert_eq!(of("    if total > 0 {", "total").role, Role::Branched);
        assert_eq!(of("    while (total) {", "total").role, Role::Branched);
        assert_eq!(of("    ok = total == expected", "ok").role, Role::Assigned);
        assert_eq!(
            of("    ok = total == expected", "total").role,
            Role::Branched
        );
        assert_eq!(
            of("    ok = total == expected", "expected").role,
            Role::Branched
        );
        assert_eq!(of("    total.round()", "total").role, Role::Member);
        assert_eq!(of("    total->cents", "total").role, Role::Member);
        assert_eq!(of("    items[total]", "items").role, Role::Member);
        assert_eq!(of("    log(total.cents)", "total").role, Role::Member);
        assert_eq!(of("    tally + total", "total").role, Role::Read);
        assert_eq!(
            of("    (total, other)", "total").role,
            Role::Read,
            "a tuple is no call"
        );
    }

    #[test]
    fn a_declarations_parameter_is_found_by_index_in_each_style() {
        assert_eq!(
            parameter_at(
                "fn render(&self, total: Money, dpi: u32) -> Png {",
                1,
                "rust"
            ),
            Some(("dpi".into(), 31))
        );
        assert_eq!(
            parameter_at("def render(self, total, dpi=2):", 0, "python"),
            Some(("total".into(), 17))
        );
        assert_eq!(
            parameter_at("func Render(total Money, dpi int) Png {", 1, "go"),
            Some(("dpi".into(), 25))
        );
        assert_eq!(
            parameter_at("public Png render(Money total, int dpi) {", 0, "java"),
            Some(("total".into(), 24))
        );
        assert_eq!(
            parameter_at("render(total: Money, dpi: number): Png {", 1, "typescript"),
            Some(("dpi".into(), 21))
        );
        assert_eq!(
            parameter_at("fn f(a: Vec<(u8, u8)>, b: u8)", 1, "rust"),
            Some(("b".into(), 23))
        );
        assert_eq!(parameter_at("fn f(a: u8)", 3, "rust"), None);
        assert_eq!(parameter_at("no parens here", 0, "rust"), None);
    }
}
