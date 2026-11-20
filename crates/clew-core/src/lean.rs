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
pub(crate) fn commands(source: &str) -> Vec<Range<usize>> {
    let mut starts = vec![0];
    // Open `/- … -/` comments (they nest) at the start of the line.
    let mut depth = 0usize;
    // Whether the last line with anything on it held attributes alone.
    let mut attributes_above = false;
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        if offset > 0 && depth == 0 && !attributes_above && starts_command(text) {
            starts.push(offset);
        }
        let opened = depth == 0;
        depth = comment_depth_after(text, depth);
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
/// before it. Block comments nest; a `--` outside one ends the line's code;
/// a `/-` in a string literal opens nothing.
fn comment_depth_after(line: &str, mut depth: usize) -> usize {
    let b = line.as_bytes();
    let mut in_string = false;
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
        if pair(b'/', b'-') {
            depth += 1;
            i += 2;
            continue;
        }
        if b[i] == b'"' {
            in_string = true;
        }
        i += 1;
    }
    depth
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
    len
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

/// How many brackets are open at the end of `text`, string literals aside.
fn bracket_depth(text: &str) -> i32 {
    let mut depth = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
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
/// string literals aside.
fn closing_bracket(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' if depth == 0 => return Some(i),
            ']' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// The Lake project the Lean file `file` belongs to: the nearest folder
/// above it, up to `root`, holding a lakefile (`lakefile.lean` or
/// `lakefile.toml`) — where Lean's server must run for the file's imports
/// of its own project to resolve, in a repository whose Lean is in a
/// folder of its own. `root` for a file in no Lake project under it.
pub fn lake_root(root: &Path, file: &Path) -> PathBuf {
    let lakefile = |dir: &Path| {
        ["lakefile.lean", "lakefile.toml"]
            .iter()
            .any(|name| dir.join(name).is_file())
    };
    file.ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(root))
        .find(|dir| lakefile(dir))
        .map_or_else(|| root.to_path_buf(), Path::to_path_buf)
}

/// Where the build of the Mathlib a Lake project at `root` depends on — or
/// is — belongs, when it is not there: `None` for a project without
/// Mathlib, and for one whose build is in place. Without it, Lean builds
/// all of Mathlib itself the first time a file importing it is checked,
/// which takes hours; `lake exe cache get` downloads it in minutes.
pub fn mathlib_build_missing(root: &Path) -> Option<PathBuf> {
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
    (!built).then_some(build)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(lake_root(&root, &proof), root.join("verification"));
        assert_eq!(
            lake_root(&root, &extracted),
            root.join("verification/extracted")
        );
        assert_eq!(lake_root(&root, &loose), root);
        // Not past the opened folder, even where a lakefile is above it.
        let inner = root.join("scratch");
        at("lakefile.lean");
        assert_eq!(lake_root(&inner, &loose), inner);
        assert_eq!(lake_root(&root, &loose), root);
    }

    #[test]
    fn an_empty_file_has_no_commands() {
        assert!(commands("").is_empty());
        assert_eq!(commands("def x := 1"), vec![0..10]);
    }
}
