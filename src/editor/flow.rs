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
    "until", "unless", "case", "with", "and", "or", "not", "in", "await", "yield", "let",
];

/// The keywords of [`KEYWORDS_BEFORE_PAREN`] whose parenthesis is a
/// condition: a value inside it is branched on.
const BRANCH_KEYWORDS: &[&str] = &[
    "if", "while", "for", "match", "switch", "elif", "catch", "except", "foreach", "until",
    "unless", "case",
];

/// The operators that assign to a name that already holds a value.
const COMPOUND_ASSIGNMENTS: &[&str] = &[
    "+=", "-=", "*=", "/=", "%=", "|=", "&=", "^=", "<<=", ">>=", "??=", "||=", "&&=", "**=", "//=",
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
        for &op in COMPOUND_ASSIGNMENTS {
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
    let first_word = line
        .trim_start()
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .unwrap_or("");
    // Passed to a call — asked before `return`: in `return f(x)` it is `f`
    // that receives the value, and what is returned is `f`'s.
    if let Some((callee, index, callee_col)) = call_before(&chars, col) {
        match callee.as_str() {
            c if BRANCH_KEYWORDS.contains(&c) => return plain(Role::Branched),
            "return" | "yield" => return plain(Role::Returned),
            c if KEYWORDS_BEFORE_PAREN.contains(&c) => {}
            _ => {
                return Use {
                    role: Role::Passed,
                    detail: callee.clone(),
                    argument: Some(index),
                    callee: Some((callee, callee_col)),
                };
            }
        }
    }
    // Returned.
    if first_word == "return" || first_word == "yield" {
        return plain(Role::Returned);
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
            rest.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == ':'))
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
    // A comma or bracket inside a string literal is text, not syntax.
    let quoted = string_mask(&chars[..i]);
    let mut open = None;
    while i > 0 {
        i -= 1;
        if quoted[i] {
            continue;
        }
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
    let quoted = string_mask(&chars);
    // The parameter list is the first `(` outside angle brackets: a generic
    // bound (`<F: Fn(u8) -> u8>`) has parentheses of its own.
    let mut angle = 0i32;
    let mut open = None;
    for (i, &c) in chars.iter().enumerate() {
        if quoted[i] {
            continue;
        }
        match c {
            '<' if opens_angle(&chars, i) => angle += 1,
            '>' if angle > 0 && closes_angle(&chars, i) => angle -= 1,
            '(' if angle == 0 => {
                open = Some(i);
                break;
            }
            _ => {}
        }
    }
    let open = open?;
    let mut depth = 0i32;
    let mut parts: Vec<(usize, usize)> = Vec::new();
    let mut part_start = open + 1;
    let mut close = chars.len();
    for (i, &c) in chars.iter().enumerate().skip(open + 1) {
        if quoted[i] {
            continue;
        }
        match c {
            '(' | '[' | '{' => depth += 1,
            '<' if opens_angle(&chars, i) => depth += 1,
            ')' if depth == 0 => {
                close = i;
                break;
            }
            ')' | ']' | '}' => depth -= 1,
            '>' if closes_angle(&chars, i) => depth -= 1,
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

/// Which chars of `chars` are inside a string or char literal (quotes
/// included): `"…"` and `` `…` `` always, and `'…'` unless the quote begins a
/// Rust lifetime (`&'a`, `<'a>`, `T: 'a`, `+ 'a`, `'a, 'b`), which nothing
/// closes. A backslash escapes the next char. An unclosed quote quotes
/// nothing.
pub fn string_mask(chars: &[char]) -> Vec<bool> {
    let mut mask = vec![false; chars.len()];
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = 0;
    while i < chars.len() {
        let q = chars[i];
        if !matches!(q, '"' | '\'' | '`') {
            i += 1;
            continue;
        }
        if q == '\'' && chars.get(i + 1).is_some_and(|&c| ident(c)) {
            let end = (i + 1..chars.len())
                .find(|&k| !ident(chars[k]))
                .unwrap_or(chars.len());
            let prev = chars[..i].iter().rev().find(|c| !c.is_whitespace());
            let lifetime = chars.get(end) != Some(&'\'')
                && (matches!(prev, Some('&' | '<' | '+' | ':'))
                    || (prev == Some(&',')
                        && end - i <= 3
                        && matches!(chars.get(end), Some(',' | '>'))));
            if lifetime {
                i = end;
                continue;
            }
        }
        let mut j = i + 1;
        let mut close = None;
        while j < chars.len() {
            match chars[j] {
                '\\' => j += 2,
                c if c == q => {
                    close = Some(j);
                    break;
                }
                _ => j += 1,
            }
        }
        match close {
            Some(j) => {
                mask[i..=j].iter_mut().for_each(|m| *m = true);
                i = j + 1;
            }
            None => i += 1,
        }
    }
    mask
}

/// Whether the `<` at `i` opens angle brackets (`Vec<T>`), not a comparison
/// or a shift (`a < b`, `a <= b`, `a << 2`).
fn opens_angle(chars: &[char], i: usize) -> bool {
    !matches!(chars.get(i + 1), None | Some('=' | '<')) && !chars[i + 1].is_whitespace()
}

/// Whether the `>` at `i` closes angle brackets, not an arrow (`->`, `=>`)
/// or a comparison (`a >= b`).
fn closes_angle(chars: &[char], i: usize) -> bool {
    !matches!(i.checked_sub(1).map(|p| chars[p]), Some('-' | '=')) && chars.get(i + 1) != Some(&'=')
}

/// Whether `line` assigns to `word` (at char column `col`) with an operator
/// that needs a value there already (`+=`, `-=`, …): a reassignment, even
/// where the language server lists the line among the name's definitions,
/// as Python's does for every assignment.
pub fn reassigns(line: &str, col: usize, word: &str) -> bool {
    let after: String = line
        .chars()
        .skip(col.saturating_add(word.chars().count()))
        .collect();
    let after = after.trim_start();
    COMPOUND_ASSIGNMENTS.iter().any(|op| after.starts_with(op))
}

/// The `index`-th (0-based) parameter of the declaration whose name starts
/// at char column `name_col` of `lines[0]`, where the parameter list may run
/// over the following lines (`fn f(\n    a: A,\n    b: B,\n)`): its
/// name, the line it is on (an offset into `lines`) and its char column on
/// that line. The search starts at the name, so a `pub(crate)` or a Go
/// receiver before it is not taken for the parameter list.
pub fn parameter_in(
    lines: &[String],
    name_col: usize,
    index: usize,
    lang: &str,
) -> Option<(String, usize, usize)> {
    let first = lines.first()?;
    let head: String = first.chars().skip(name_col).collect();
    let mut joined = head;
    for l in &lines[1..] {
        joined.push('\n');
        joined.push_str(l);
    }
    let (name, col) = parameter_at(&joined, index, lang)?;
    // Back from a column of `joined` to a line and a column on it.
    let before: Vec<char> = joined.chars().take(col).collect();
    let line = before.iter().filter(|&&c| c == '\n').count();
    let on_line = match before.iter().rposition(|&c| c == '\n') {
        Some(nl) => col - nl - 1,
        None => col + name_col,
    };
    Some((name, line, on_line))
}

/// The line of `lines` whose trimmed text is `text`, nearest to `old`: where
/// a traced occurrence's line went after the file changed. `None` when no
/// line reads so any more.
pub fn moved_line<S: AsRef<str>>(lines: &[S], text: &str, old: usize) -> Option<usize> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let n = lines.len();
    let is = |i: usize| lines[i].as_ref().trim() == text;
    (0..n.max(old + 1)).find_map(|d| {
        let below = old + d;
        if below < n && is(below) {
            return Some(below);
        }
        old.checked_sub(d).filter(|&above| above < n && is(above))
    })
}

/// Where a traced line whose text is gone most likely went, in a file now
/// `len` lines long: `old` moved by as much as its nearest neighbour that
/// was found again (the closest one above, else the closest below), each
/// given in `anchors` as its `(old, new)` line. With no neighbour found, the
/// line stays put. `None` when the file is empty.
pub fn carried_line(anchors: &[(usize, usize)], old: usize, len: usize) -> Option<usize> {
    let last = len.checked_sub(1)?;
    let above = anchors.iter().filter(|a| a.0 <= old).max_by_key(|a| a.0);
    let below = || anchors.iter().filter(|a| a.0 > old).min_by_key(|a| a.0);
    let line = match above.or_else(below) {
        Some(&(from, to)) => (old + to).saturating_sub(from),
        None => old,
    };
    Some(line.min(last))
}

/// The char column of the whole-word occurrence of `word` in `line` nearest
/// to column `near`.
pub fn nearest_word(line: &str, word: &str, near: usize) -> Option<usize> {
    let chars: Vec<char> = line.chars().collect();
    let w: Vec<char> = word.chars().collect();
    if w.is_empty() || w.len() > chars.len() {
        return None;
    }
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    (0..=chars.len() - w.len())
        .filter(|&i| chars[i..i + w.len()] == w[..])
        .filter(|&i| i == 0 || !ident(chars[i - 1]))
        .filter(|&i| i + w.len() == chars.len() || !ident(chars[i + w.len()]))
        .min_by_key(|&i| i.abs_diff(near))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_handed_to_a_call_on_a_return_line_is_passed() {
        let u = of("    return apply_discount(subtotal, discount)", "subtotal");
        assert_eq!(u.role, Role::Passed);
        assert_eq!(u.detail, "apply_discount");
        assert_eq!(u.argument, Some(0));
        let line = "    return apply_discount(subtotal, discount)";
        let u = classify(line, line.rfind("discount").unwrap(), "discount");
        assert_eq!((u.role, u.argument), (Role::Passed, Some(1)));
        assert_eq!(of("    return subtotal", "subtotal").role, Role::Returned);
        assert_eq!(of("    return (subtotal)", "subtotal").role, Role::Returned);
        assert_eq!(of("    yield f(x)", "x").role, Role::Passed);
        assert_eq!(of("if (ready) {", "ready").role, Role::Branched);
        assert_eq!(of("while (n > 0) {", "n").role, Role::Branched);
        assert_eq!(of("    print(total)", "total").role, Role::Passed);
    }

    #[test]
    fn a_compound_assignment_reassigns() {
        let line = "        subtotal += line_total(line)";
        assert!(reassigns(line, 8, "subtotal"));
        assert!(reassigns("x //= 2", 0, "x"));
        assert!(!reassigns("    subtotal = 0", 4, "subtotal"));
        assert!(!reassigns("    if subtotal == 0:", 7, "subtotal"));
    }

    #[test]
    fn a_parameter_list_over_several_lines_is_read_from_the_name_on() {
        let lines: Vec<String> = [
            "pub(crate) fn save_markdown(",
            "    window: Option<iced::window::Id>,",
            "    file_name: String,",
            ") -> Task<Option<PathBuf>> {",
        ]
        .map(String::from)
        .to_vec();
        let name_col = "pub(crate) fn ".chars().count();
        assert_eq!(
            parameter_in(&lines, name_col, 1, "rust"),
            Some(("file_name".into(), 2, 4))
        );
        assert_eq!(
            parameter_in(&lines, name_col, 0, "rust"),
            Some(("window".into(), 1, 4))
        );
        assert_eq!(parameter_in(&lines, name_col, 2, "rust"), None);
        // One line, with a `pub(crate)` before the name.
        let one = vec!["pub(crate) fn f(a: u8, b: u8) {".to_string()];
        assert_eq!(
            parameter_in(&one, "pub(crate) fn ".len(), 1, "rust"),
            Some(("b".into(), 0, 23))
        );
        // A Go receiver before the name is not the parameter list.
        let go = vec!["func (s *Shop) Total(order Order, rate int) int {".to_string()];
        assert_eq!(
            parameter_in(&go, "func (s *Shop) ".len(), 1, "go"),
            Some(("rate".into(), 0, 34))
        );
        let py: Vec<String> = [
            "def apply_discount(",
            "    subtotal: int,",
            "    discount: Discount,",
            ") -> int:",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            parameter_in(&py, 4, 0, "python"),
            Some(("subtotal".into(), 1, 4))
        );
    }

    #[test]
    fn a_moved_line_is_found_nearest_to_where_it_was() {
        let lines = [
            "",
            "a = 1",
            "x",
            "    subtotal = 0",
            "y",
            "    subtotal = 0",
        ];
        assert_eq!(moved_line(&lines, "subtotal = 0", 2), Some(3));
        assert_eq!(moved_line(&lines, "subtotal = 0", 5), Some(5));
        assert_eq!(moved_line(&lines, "subtotal = 0", 40), Some(5));
        assert_eq!(moved_line(&lines, "gone", 1), None);
        assert_eq!(moved_line(&lines, "", 1), None);
        assert_eq!(nearest_word("ab a_b ab", "ab", 8), Some(7));
        assert_eq!(nearest_word("ab a_b ab", "ab", 0), Some(0));
        assert_eq!(nearest_word("abc", "ab", 0), None);
    }

    #[test]
    fn arrows_generics_and_strings_do_not_break_a_parameter_list() {
        // An arrow's `>` closes nothing.
        assert_eq!(
            parameter_at("apply(cb: () => void, x: number) {", 1, "typescript"),
            Some(("x".into(), 22))
        );
        assert_eq!(
            parameter_at("apply(f: impl Fn(u8) -> u8, x: u8) {", 1, "rust"),
            Some(("x".into(), 28))
        );
        // A generic bound's parentheses are not the parameter list.
        assert_eq!(
            parameter_at("apply<F: Fn(u8) -> u8>(f: F, x: u8) {", 1, "rust"),
            Some(("x".into(), 29))
        );
        assert_eq!(
            parameter_at("join(xs: Vec<String>, sep: &str)", 1, "rust"),
            Some(("sep".into(), 22))
        );
        // A comma in a default string is text; a comparison is no bracket.
        assert_eq!(
            parameter_at("def show(items, sep=\", \", limit=n < 3):", 2, "python"),
            Some(("limit".into(), 26))
        );
        // A lifetime is no string.
        assert_eq!(
            parameter_at("pick<'a>(a: &'a str, b: &'a str) -> &'a str {", 1, "rust"),
            Some(("b".into(), 21))
        );
    }

    #[test]
    fn a_comma_in_a_string_is_not_an_argument_separator() {
        let line = "printf(\"%d, %d\\n\", a, b);";
        let u = classify(line, line.find(", a").unwrap() + 2, "a");
        assert_eq!((u.role, u.argument), (Role::Passed, Some(1)));
        let line = "log('a, b', x)";
        let u = classify(line, line.find("x)").unwrap(), "x");
        assert_eq!(u.argument, Some(1));
        let line = "f(',', x)";
        assert_eq!(classify(line, 7, "x").argument, Some(1), "a char literal");
        let mask = string_mask(
            &"fn f<'a, 'b>(x: &'a T, y: &'b T)"
                .chars()
                .collect::<Vec<_>>(),
        );
        assert!(!mask.iter().any(|&m| m), "lifetimes quote nothing");
    }

    #[test]
    fn an_awaited_or_constructed_value_names_its_call() {
        let line = "const data = (await res.json()).items";
        assert_eq!(classify(line, 6, "data").detail, "json()");
        assert_eq!(classify("x = (new Foo()).bar", 0, "x").detail, "Foo()");
        assert_eq!(
            classify("let user = await fetchUser(id);", 4, "user").detail,
            "fetchUser()"
        );
    }

    #[test]
    fn a_rewritten_line_moves_with_its_nearest_found_neighbour() {
        // Lines 3 and 9 were found again two lines down and one line up.
        let anchors = [(3, 5), (9, 8)];
        assert_eq!(carried_line(&anchors, 4, 20), Some(6), "follows 3 → 5");
        assert_eq!(carried_line(&anchors, 12, 20), Some(11), "follows 9 → 8");
        assert_eq!(carried_line(&anchors, 1, 20), Some(3), "only 3 is near");
        assert_eq!(carried_line(&anchors, 4, 5), Some(4), "kept in the file");
        assert_eq!(carried_line(&[(3, 0)], 1, 20), Some(0), "never above 0");
        assert_eq!(carried_line(&[], 7, 20), Some(7), "nothing to follow");
        assert_eq!(carried_line(&anchors, 4, 0), None, "the file is empty");
    }

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
