//! What can be run in a source file, and where: its tests, each with the
//! scopes it is named through (Rust's inline modules, Python's classes),
//! and the program's start — what Run and Debug offer at a line (the
//! gutter's ▶, the code menu).
//!
//! Read from the file alone, with one parse: a test is a test as the symbol
//! index calls it ([`EntryPoints::is_test`]), so the gutter never marks one
//! the outline does not.

use tree_sitter::Node;

use crate::highlight::{Lang, parse};
use crate::outline::EntryPoints;

mod params;

/// What a runnable is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnableKind {
    /// A test: run on its own, by its test runner. A parametrized Python
    /// test's case is one too, named as pytest names it (`test_add[1-2]`).
    Test,
    /// A group of tests, run together: a Python test class's or a Rust
    /// module's (named for it), or a test file's (its name empty: the
    /// file).
    Tests,
    /// The program's start: Rust's `fn main`, Python's
    /// `if __name__ == "__main__":` block, Lean's `def main`.
    Main,
    /// A Rust doc comment's example, run by rustdoc as a test of its own
    /// (`cargo test --doc`): named for the item it documents (empty for a
    /// module's), its mark on the line its code block opens.
    Doctest,
}

/// Something in a file that can be run on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runnable {
    pub kind: RunnableKind,
    /// Its function's name (`adds`, `main`); `__main__` for Python's block;
    /// a test class's for its tests, empty for a file's.
    pub name: String,
    /// The 1-based line its mark goes on: its name's, or the block's `if`.
    pub line: usize,
    /// The 1-based lines it spans, inclusive: where the cursor is in it.
    pub span: (usize, usize),
    /// What it is named through, outermost first: the inline modules
    /// around a Rust test (`tests`), the classes around a Python one
    /// (`TestPrices`).
    pub scopes: Vec<String>,
}

/// Most scopes a runnable is named through; deeper, it is not offered (no
/// one nests a test in more).
const MAX_SCOPES: usize = 32;

/// The runnables of `source`, a file at `rel` in language `lang` (a key):
/// [`runnables`]', and the tests of the languages whose test files are
/// told by their names too ([`crate::langtests`]: Go's `_test.go`,
/// RSpec's `_spec.rb`, Common Test's `_SUITE.erl`).
pub fn runnables_in(lang: &str, rel: &str, source: &str) -> Vec<Runnable> {
    match crate::langtests::handles(lang) {
        true => crate::langtests::tests(lang, rel, source),
        false => runnables(lang, source),
    }
}

/// The runnables of `source`, in language `lang` (a key), in order.
/// Rust and Python have them, JavaScript and TypeScript their tests, and
/// Lean its `main`; other languages none yet.
pub fn runnables(lang: &str, source: &str) -> Vec<Runnable> {
    let language = match lang {
        "javascript" | "typescript" | "tsx" => return ecma_tests(lang, source),
        "rust" => Lang::Rust,
        "python" => Lang::Python,
        "lean" => {
            return crate::lean::main_lines(source)
                .map(|(line, span)| Runnable {
                    kind: RunnableKind::Main,
                    name: "main".to_string(),
                    line,
                    span,
                    scopes: Vec::new(),
                })
                .into_iter()
                .collect();
        }
        _ => return Vec::new(),
    };
    let Some(tree) = parse(source, language) else {
        return Vec::new();
    };
    let lines: Vec<&str> = source.lines().collect();
    let entries = EntryPoints::new(&lines, lang, "");
    let mut out = Vec::new();
    let mut scopes = Vec::new();
    match language {
        Lang::Rust => {
            rust(tree.root_node(), source, &entries, &mut scopes, &mut out);
            out.extend(rust_doctests(&lines));
        }
        _ => {
            let root = tree.root_node();
            let parametrized = marks_parametrize(root, source);
            python(root, source, &entries, &mut scopes, parametrized, &mut out);
        }
    }
    // A test file's tests, all of them: what Run Test runs from a line in
    // no test.
    if out.iter().any(|r| r.kind == RunnableKind::Test) {
        out.insert(
            0,
            Runnable {
                kind: RunnableKind::Tests,
                name: String::new(),
                line: 1,
                span: (1, lines.len().max(1)),
                scopes: Vec::new(),
            },
        );
    }
    // A Rust file's doctests in order among the rest (stable: the file's
    // own group stays first).
    if language == Lang::Rust {
        out.sort_by_key(|r| r.line);
    }
    out
}

/// A JavaScript or TypeScript file's tests, as its outline has them
/// (`describe`, `it`, `test`, `Deno.test`, …): each test named by its title
/// through the suites around it, each suite a group of tests, and the file
/// all of them.
fn ecma_tests(lang: &str, source: &str) -> Vec<Runnable> {
    let symbols = crate::outline::extract(source, lang);
    let suites: Vec<&crate::outline::Symbol> =
        symbols.iter().filter(|s| s.kind == "suite").collect();
    let mut out: Vec<Runnable> = symbols
        .iter()
        .filter(|s| matches!(s.kind.as_str(), "suite" | "test"))
        .map(|s| {
            // The suites that hold it, outermost first.
            let scopes = suites
                .iter()
                .filter(|o| {
                    !std::ptr::eq(**o, s)
                        && o.line <= s.line
                        && s.end_line <= o.end_line
                        && (o.line, o.end_line) != (s.line, s.end_line)
                })
                .take(MAX_SCOPES)
                .map(|o| o.name.clone())
                .collect();
            Runnable {
                kind: match s.kind.as_str() {
                    "suite" => RunnableKind::Tests,
                    _ => RunnableKind::Test,
                },
                name: s.name.clone(),
                line: s.line,
                span: (s.line, s.end_line.max(s.line)),
                scopes,
            }
        })
        .collect();
    out.sort_by_key(|r| (r.line, r.span.1 == r.span.0));
    if out.iter().any(|r| r.kind == RunnableKind::Test) {
        out.insert(
            0,
            Runnable {
                kind: RunnableKind::Tests,
                name: String::new(),
                line: 1,
                span: (1, source.lines().count().max(1)),
                scopes: Vec::new(),
            },
        );
    }
    out
}

/// Most doctests one file offers: a file of more examples than this is
/// generated, and read for none.
const MAX_DOCTESTS: usize = 2_000;

/// The examples of a Rust file's doc comments (`///`, `//!`) that rustdoc
/// runs as tests: a fenced code block with no language, `rust`, or only
/// rustdoc's own words (`should_panic`, `no_run`, `compile_fail`,
/// `edition2021` …). Not `ignore`d ones, nor another language's (`text`,
/// `toml`, `sh`).
fn rust_doctests(lines: &[&str]) -> Vec<Runnable> {
    let doc = |l: &str| -> Option<(bool, String)> {
        let t = l.trim_start();
        if let Some(rest) = t.strip_prefix("//!") {
            return Some((true, rest.strip_prefix(' ').unwrap_or(rest).to_string()));
        }
        let rest = t.strip_prefix("///").filter(|r| !r.starts_with('/'))?;
        Some((false, rest.strip_prefix(' ').unwrap_or(rest).to_string()))
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() && out.len() < MAX_DOCTESTS {
        let Some((inner, text)) = doc(lines[i]) else {
            i += 1;
            continue;
        };
        let fence = text.trim_start();
        let marker = match () {
            _ if fence.starts_with("```") => "```",
            _ if fence.starts_with("~~~") => "~~~",
            _ => {
                i += 1;
                continue;
            }
        };
        let info = fence.trim_start_matches(marker.chars().next().unwrap_or('`'));
        let opened = i;
        // Its end: the fence that closes it, in the same comment.
        let mut j = i + 1;
        let mut closed = None;
        while let Some((_, t)) = lines.get(j).and_then(|l| doc(l)) {
            if t.trim_start().starts_with(marker) {
                closed = Some(j);
                break;
            }
            j += 1;
        }
        let Some(closed) = closed else {
            i = j;
            continue;
        };
        i = closed + 1;
        if !runs_as_rust(info) {
            continue;
        }
        // What it documents: the item below the comment (its name), or —
        // an inner doc — the module itself (none).
        let name = match inner {
            true => String::new(),
            false => documented_item(lines, closed + 1).unwrap_or_default(),
        };
        out.push(Runnable {
            kind: RunnableKind::Doctest,
            name,
            line: opened + 1,
            span: (opened + 1, closed + 1),
            scopes: Vec::new(),
        });
    }
    out
}

/// Whether a code block's info string (what follows its opening fence) is
/// one rustdoc runs as Rust: empty, or only `rust` and rustdoc's own words.
fn runs_as_rust(info: &str) -> bool {
    info.split([',', ' ', '\t'])
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .all(|w| {
            matches!(
                w,
                "rust"
                    | "should_panic"
                    | "no_run"
                    | "compile_fail"
                    | "test_harness"
                    | "standalone_crate"
                    | "allow_fail"
            ) || w.starts_with("edition")
                || (w.starts_with("E0") && w[1..].chars().all(|c| c.is_ascii_digit()))
        })
}

/// The name of the item a doc comment ending on the line before 0-based
/// `from` documents: the first line past its comments and attributes, its
/// name after its keyword (`fn`, `struct`, `mod`, …).
fn documented_item(lines: &[&str], from: usize) -> Option<String> {
    const KEYWORDS: &[&str] = &[
        "fn",
        "struct",
        "enum",
        "trait",
        "mod",
        "type",
        "const",
        "static",
        "union",
        "macro_rules!",
    ];
    let line = lines
        .iter()
        .skip(from)
        .take(64)
        .map(|l| l.trim())
        .find(|t| !(t.is_empty() || t.starts_with("//") || t.starts_with("#[")))?;
    let words: Vec<&str> = line
        .split(|c: char| {
            c.is_whitespace() || c == '(' || c == '<' || c == '{' || c == ':' || c == ';'
        })
        .filter(|w| !w.is_empty())
        .collect();
    let at = words.iter().position(|w| KEYWORDS.contains(w))?;
    let name = words.get(at + 1)?;
    name.chars()
        .all(|c| c == '_' || c.is_alphanumeric())
        .then(|| name.to_string())
}

/// Whether a Python block (a module, a class's body) parametrizes all its
/// tests with `pytestmark = pytest.mark.parametrize(…)`: their cases' ids
/// then combine with it, and are not known from a test alone.
fn marks_parametrize(block: Node, source: &str) -> bool {
    let mut cursor = block.walk();
    block.named_children(&mut cursor).any(|statement| {
        statement.kind() == "expression_statement"
            && statement
                .named_child(0)
                .filter(|a| a.kind() == "assignment")
                .is_some_and(|a| {
                    a.child_by_field_name("left")
                        .is_some_and(|l| text(l, source) == "pytestmark")
                        && text(a, source).contains("parametrize")
                })
    })
}

fn text<'a>(node: Node, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

fn span(node: Node) -> (usize, usize) {
    (node.start_position().row + 1, node.end_position().row + 1)
}

/// A Rust item list (a file, an inline module's body): its tests, and a
/// top-level `fn main`, through its inline modules. Not inside a function,
/// an `impl` or a trait: nothing there is a test the harness runs.
fn rust(
    list: Node,
    source: &str,
    entries: &EntryPoints,
    scopes: &mut Vec<String>,
    out: &mut Vec<Runnable>,
) {
    let mut cursor = list.walk();
    for item in list.named_children(&mut cursor) {
        match item.kind() {
            "function_item" => {
                let Some(name) = item.child_by_field_name("name") else {
                    continue;
                };
                let name_text = text(name, source);
                let line = name.start_position().row + 1;
                let kind = if entries.is_test(line, name_text) {
                    Some(RunnableKind::Test)
                } else if scopes.is_empty() && name_text == "main" {
                    Some(RunnableKind::Main)
                } else {
                    None
                };
                if let Some(kind) = kind {
                    out.push(Runnable {
                        kind,
                        name: name_text.to_string(),
                        line,
                        span: span(item),
                        scopes: scopes.clone(),
                    });
                }
            }
            // proptest's tests are written in its macro, each a `#[test]`
            // `fn`: read off the macro's tokens.
            "macro_invocation"
                if item
                    .child_by_field_name("macro")
                    .is_some_and(|m| text(m, source).ends_with("proptest")) =>
            {
                proptest_tests(item, source, scopes, out);
            }
            "mod_item" if scopes.len() < MAX_SCOPES => {
                let (Some(name), Some(body)) = (
                    item.child_by_field_name("name"),
                    item.child_by_field_name("body"),
                ) else {
                    continue;
                };
                let name_text = text(name, source);
                let first = out.len();
                scopes.push(name_text.to_string());
                rust(body, source, entries, scopes, out);
                scopes.pop();
                // The module's tests, all of them, when it has any: a
                // `mod tests` run as a whole.
                if out[first..].iter().any(|r| r.kind == RunnableKind::Test) {
                    out.insert(
                        first,
                        Runnable {
                            kind: RunnableKind::Tests,
                            name: name_text.to_string(),
                            line: name.start_position().row + 1,
                            span: span(item),
                            scopes: scopes.clone(),
                        },
                    );
                }
            }
            _ => {}
        }
    }
}

/// The tests of a `proptest! { … }`: each `#[test] fn name(…) { … }` in its
/// tokens, named as proptest names the test it makes of it.
fn proptest_tests(call: Node, source: &str, scopes: &[String], out: &mut Vec<Runnable>) {
    let mut cursor = call.walk();
    let Some(body) = call
        .children(&mut cursor)
        .find(|c| c.kind() == "token_tree")
    else {
        return;
    };
    let mut cursor = body.walk();
    let tokens: Vec<Node> = body.children(&mut cursor).collect();
    for (i, t) in tokens.iter().enumerate() {
        if text(*t, source) != "#"
            || tokens
                .get(i + 1)
                .is_none_or(|a| text(*a, source).split_whitespace().collect::<String>() != "[test]")
        {
            continue;
        }
        // Past any further attributes, to the `fn` and its name.
        let Some(at) = (i + 2..tokens.len().min(i + 32)).find(|&k| text(tokens[k], source) == "fn")
        else {
            continue;
        };
        let Some(name) = tokens.get(at + 1).filter(|n| n.kind() == "identifier") else {
            continue;
        };
        let end = tokens[at..]
            .iter()
            .find(|n| n.kind() == "token_tree" && text(**n, source).starts_with('{'))
            .map_or(*name, |b| *b);
        out.push(Runnable {
            kind: RunnableKind::Test,
            name: text(*name, source).to_string(),
            line: name.start_position().row + 1,
            span: (t.start_position().row + 1, end.end_position().row + 1),
            scopes: scopes.to_vec(),
        });
    }
}

/// A Python block (a module, a class's body): its tests, through its
/// classes, and a module's `if __name__ == "__main__":` block. Not inside
/// a function: pytest collects nothing there.
fn python(
    block: Node,
    source: &str,
    entries: &EntryPoints,
    scopes: &mut Vec<String>,
    parametrized: bool,
    out: &mut Vec<Runnable>,
) {
    let mut cursor = block.walk();
    for statement in block.named_children(&mut cursor) {
        // `@decorator\ndef …` / `@decorator\nclass …`: the definition.
        let item = match statement.kind() {
            "decorated_definition" => match statement.child_by_field_name("definition") {
                Some(def) => def,
                None => continue,
            },
            _ => statement,
        };
        match item.kind() {
            "function_definition" => {
                let Some(name) = item.child_by_field_name("name") else {
                    continue;
                };
                let name_text = text(name, source);
                let line = name.start_position().row + 1;
                // Only test classes are walked into (below): a method here
                // is one pytest collects.
                if entries.is_test(line, name_text) {
                    out.push(Runnable {
                        kind: RunnableKind::Test,
                        name: name_text.to_string(),
                        line,
                        span: span(statement),
                        scopes: scopes.clone(),
                    });
                    // Each case of a parametrized test, on its own lines.
                    if statement.kind() == "decorated_definition" && !parametrized {
                        for case in params::cases(statement, source) {
                            out.push(Runnable {
                                kind: RunnableKind::Test,
                                name: format!("{name_text}[{}]", case.id),
                                line: case.span.0,
                                span: case.span,
                                scopes: scopes.clone(),
                            });
                        }
                    }
                }
            }
            "class_definition" if scopes.len() < MAX_SCOPES => {
                let (Some(name), Some(body)) = (
                    item.child_by_field_name("name"),
                    item.child_by_field_name("body"),
                ) else {
                    continue;
                };
                let name_text = text(name, source);
                let bases = item
                    .child_by_field_name("superclasses")
                    .map(|b| text(b, source))
                    .unwrap_or("");
                // pytest's own rule: a class named `Test…`, or unittest's
                // `TestCase` whatever its name.
                if !(name_text.starts_with("Test") || bases.contains("TestCase")) {
                    continue;
                }
                // A class parametrized as a whole gives its tests' cases
                // its own ids too.
                let parametrized = parametrized
                    || marks_parametrize(body, source)
                    || (statement.kind() == "decorated_definition"
                        && text(statement, source)
                            .get(..item.start_byte() - statement.start_byte())
                            .is_some_and(|decorators| decorators.contains("parametrize")));
                let first = out.len();
                scopes.push(name_text.to_string());
                python(body, source, entries, scopes, parametrized, out);
                scopes.pop();
                // The class's tests, all of them, when it has any.
                if out.len() > first {
                    out.insert(
                        first,
                        Runnable {
                            kind: RunnableKind::Tests,
                            name: name_text.to_string(),
                            line: name.start_position().row + 1,
                            span: span(statement),
                            scopes: scopes.clone(),
                        },
                    );
                }
            }
            "if_statement" if scopes.is_empty() => {
                let Some(condition) = item.child_by_field_name("condition") else {
                    continue;
                };
                let condition: String = text(condition, source)
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                if matches!(
                    condition.as_str(),
                    "__name__==\"__main__\""
                        | "__name__=='__main__'"
                        | "\"__main__\"==__name__"
                        | "'__main__'==__name__"
                ) {
                    out.push(Runnable {
                        kind: RunnableKind::Main,
                        name: "__main__".into(),
                        line: item.start_position().row + 1,
                        span: span(item),
                        scopes: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }
}

/// The runnable at 1-based `line`: the innermost whose span holds it.
pub fn runnable_at(runnables: &[Runnable], line: usize) -> Option<&Runnable> {
    // Of two that span the same lines — a file's tests and the one suite
    // or class that fills it — the one listed later, inside the other.
    runnables
        .iter()
        .rev()
        .filter(|r| r.span.0 <= line && line <= r.span.1)
        .min_by_key(|r| r.span.1 - r.span.0)
}

/// A Python test's pytest node id: its file as given (`tests/test_x.py`,
/// slashes), then its classes and its name, `::` between. A file's tests
/// are the file's.
pub fn pytest_node(rel: &str, runnable: &Runnable) -> String {
    let mut node = rel.replace('\\', "/");
    for scope in &runnable.scopes {
        node.push_str("::");
        node.push_str(scope);
    }
    if !runnable.name.is_empty() {
        node.push_str("::");
        node.push_str(&runnable.name);
    }
    node
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(found: &[Runnable]) -> Vec<(RunnableKind, String, usize, Vec<String>)> {
        found
            .iter()
            .map(|r| (r.kind, r.name.clone(), r.line, r.scopes.clone()))
            .collect()
    }

    /// A doc comment's examples that rustdoc runs are doctests, named for
    /// what they document: not another language's block, nor an ignored
    /// one; a module's own (`//!`) has no name.
    #[test]
    fn a_rust_files_doc_examples_are_doctests() {
        let source = "\
//! The crate.
//!
//! ```
//! assert!(true);
//! ```
/// Adds.
///
/// ```rust,should_panic
/// panic!();
/// ```
/// ```text
/// not code
/// ```
/// ```ignore
/// skipped();
/// ```
#[inline]
pub fn add(a: i32) -> i32 { a }
/// ~~~
/// let x = 1;
/// ~~~
pub struct Point<T>;
";
        let found = runnables("rust", source);
        let doctests: Vec<(String, usize, (usize, usize))> = found
            .iter()
            .filter(|r| r.kind == RunnableKind::Doctest)
            .map(|r| (r.name.clone(), r.line, r.span))
            .collect();
        assert_eq!(
            doctests,
            [
                (String::new(), 3, (3, 5)),
                ("add".into(), 8, (8, 10)),
                ("Point".into(), 19, (19, 21)),
            ]
        );
        assert_eq!(
            runnable_at(&found, 9).map(|r| (r.kind, r.name.as_str())),
            Some((RunnableKind::Doctest, "add"))
        );
        assert!(
            !found.iter().any(|r| r.kind == RunnableKind::Tests),
            "{found:?}"
        );
    }

    /// A Lean file's `main` is its program's start, its ▶ on its name.
    #[test]
    fn a_lean_main_is_the_programs_start() {
        let found = runnables(
            "lean",
            "def greet := \"hi\"\n\ndef main : IO Unit :=\n  IO.println greet\n",
        );
        assert_eq!(
            found,
            [Runnable {
                kind: RunnableKind::Main,
                name: "main".into(),
                line: 3,
                span: (3, 4),
                scopes: Vec::new(),
            }]
        );
        assert_eq!(runnable_at(&found, 4).map(|r| r.line), Some(3));
        assert!(runnables("lean", "theorem t : 1 = 1 := rfl\n").is_empty());
    }

    /// proptest's tests, written in its macro, and quickcheck's, are tests.
    #[test]
    fn proptest_and_quickcheck_tests_are_found() {
        let source = "\
#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn round_trips(x in 0..10u8) {
            prop_assert_eq!(x, x);
        }
        fn helper() {}
    }
    #[quickcheck]
    fn reverses(xs: Vec<u8>) -> bool { true }
}
";
        let found = runnables("rust", source);
        let tests: Vec<(&str, usize, Vec<String>)> = found
            .iter()
            .filter(|r| r.kind == RunnableKind::Test)
            .map(|r| (r.name.as_str(), r.line, r.scopes.clone()))
            .collect();
        assert_eq!(
            tests,
            [
                ("round_trips", 6, vec!["tests".to_string()]),
                ("reverses", 12, vec!["tests".to_string()])
            ]
        );
        assert_eq!(
            runnable_at(&found, 7).map(|r| r.name.as_str()),
            Some("round_trips")
        );
    }

    /// A Rust file's tests, through its inline modules, and its `main`.
    #[test]
    fn a_rust_files_tests_and_main_are_found_with_their_modules() {
        let source = "\
fn main() {
    println!(\"hi\");
}

fn helper() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(1 + 1, 2);
    }

    mod deeper {
        #[tokio::test]
        async fn waits() {}
    }

    fn not_a_test() {}
}
";
        let found = runnables("rust", source);
        assert_eq!(
            named(&found),
            [
                (RunnableKind::Tests, String::new(), 1, vec![]),
                (RunnableKind::Main, "main".into(), 1, vec![]),
                (RunnableKind::Tests, "tests".into(), 8, vec![]),
                (
                    RunnableKind::Test,
                    "adds".into(),
                    12,
                    vec!["tests".to_string()]
                ),
                (
                    RunnableKind::Tests,
                    "deeper".into(),
                    16,
                    vec!["tests".to_string()]
                ),
                (
                    RunnableKind::Test,
                    "waits".into(),
                    18,
                    vec!["tests".to_string(), "deeper".to_string()]
                ),
            ]
        );
        // The innermost runnable at a line: the test the cursor is in.
        assert_eq!(
            runnable_at(&found, 13).map(|r| r.name.as_str()),
            Some("adds")
        );
        assert_eq!(
            runnable_at(&found, 2).map(|r| r.name.as_str()),
            Some("main")
        );
        // Out of any test: the module's tests, or the file's.
        assert_eq!(
            runnable_at(&found, 21).map(|r| (r.kind, r.name.as_str())),
            Some((RunnableKind::Tests, "tests")),
            "in the module, no test"
        );
        assert_eq!(
            runnable_at(&found, 5).map(|r| (r.kind, r.name.as_str())),
            Some((RunnableKind::Tests, "")),
            "a plain function"
        );
    }

    /// A Python file's tests — functions, and methods of test classes —
    /// and its `__main__` block; pytest's node id names one.
    #[test]
    fn a_python_files_tests_and_main_block_are_found_with_their_classes() {
        let source = "\
import unittest
import pytest


def test_adds():
    assert 1 + 1 == 2


@pytest.mark.slow
def test_slow():
    pass


def helper():
    pass


class TestPrices:
    def test_total(self):
        pass

    def helper(self):
        pass


class Checks(unittest.TestCase):
    def test_empty(self):
        self.assertTrue(True)


class Plain:
    def test_not_collected(self):
        pass


if __name__ == \"__main__\":
    unittest.main()
";
        let found = runnables("python", source);
        assert_eq!(
            named(&found),
            [
                (RunnableKind::Tests, "".into(), 1, vec![]),
                (RunnableKind::Test, "test_adds".into(), 5, vec![]),
                (RunnableKind::Test, "test_slow".into(), 10, vec![]),
                (RunnableKind::Tests, "TestPrices".into(), 18, vec![]),
                (
                    RunnableKind::Test,
                    "test_total".into(),
                    19,
                    vec!["TestPrices".to_string()]
                ),
                (RunnableKind::Tests, "Checks".into(), 26, vec![]),
                (
                    RunnableKind::Test,
                    "test_empty".into(),
                    27,
                    vec!["Checks".to_string()]
                ),
                (RunnableKind::Main, "__main__".into(), 36, vec![]),
            ]
        );
        // Outside a test, its class's tests; outside a class, the file's.
        let class = runnable_at(&found, 22).unwrap();
        assert_eq!(
            (class.kind, class.name.as_str()),
            (RunnableKind::Tests, "TestPrices")
        );
        assert_eq!(pytest_node("test_x.py", class), "test_x.py::TestPrices");
        let file = runnable_at(&found, 15).unwrap();
        assert_eq!((file.kind, file.name.as_str()), (RunnableKind::Tests, ""));
        assert_eq!(pytest_node("test_x.py", file), "test_x.py");
        assert_eq!(runnable_at(&found, 37).unwrap().kind, RunnableKind::Main);
        // A file with no test has no tests to run.
        assert!(runnables("python", "def helper():\n    pass\n").is_empty());
        let total = runnable_at(&found, 20).unwrap();
        assert_eq!(
            pytest_node("tests\\test_prices.py", total),
            "tests/test_prices.py::TestPrices::test_total"
        );
        assert_eq!(
            pytest_node("test_x.py", runnable_at(&found, 11).unwrap()),
            "test_x.py::test_slow",
            "a decorated test, its decorator's line included"
        );
        // Other languages have none yet.
        assert!(runnables("go", "func TestX(t *testing.T) {}").is_empty());
    }

    /// A parametrized test's cases, each by the id pytest gives it, on the
    /// lines it is written on: a case is run alone from its line, the test
    /// from its own. A case whose id is not known from the source, or that
    /// shares its line, is run with the rest.
    #[test]
    fn a_parametrized_tests_cases_are_named_as_pytest_names_them() {
        let source = "\
import pytest
from enum import Enum


@pytest.mark.parametrize(\"a, b\", [
    (1, 2),
    (-3, 0.5),
    pytest.param(4, 5, id=\"big\"),
    (\"x\", None),
    ([1], True),
    (Color.RED, 2),
    (1, 2),
])
def test_add(a, b):
    pass


@pytest.mark.parametrize(\"n\", [1, 2, 3])
def test_one_line(n):
    pass


@pytest.mark.parametrize(
    [\"word\"],
    [
        (\"hello\",),
        (\"bye\",),
    ],
    ids=[\"greet\", None],
)
def test_ids(word):
    pass


@pytest.mark.parametrize(\"n\", [1, 2])
@pytest.mark.parametrize(\"m\", [3, 4])
def test_stacked(n, m):
    pass


class TestShapes:
    @pytest.mark.parametrize(\"side\", [
        1.0,
        0.00001,
    ])
    def test_area(self, side):
        pass


@pytest.mark.parametrize(\"v\", [
    1e16,
    -0.0,
    0x1F,
    -7,
    \"a b\",
    r\"raw\",
    {\"k\": 1},
    True,
    None,
])
def test_values(v):
    pass
";
        let found = runnables("python", source);
        let tests: Vec<(String, usize)> = found
            .iter()
            .filter(|r| r.kind == RunnableKind::Test)
            .map(|r| (r.name.clone(), r.line))
            .collect();
        let expected: Vec<(String, usize)> = [
            ("test_add", 14),
            // The two `(1, 2)`s share their id: pytest renames both.
            ("test_add[-3-0.5]", 7),
            ("test_add[big]", 8),
            ("test_add[x-None]", 9),
            ("test_add[a4-True]", 10),
            // `Color.RED` may be an enum, named by its value: no id.
            ("test_one_line", 19),
            ("test_ids", 31),
            ("test_ids[greet]", 26),
            ("test_ids[bye]", 27),
            ("test_stacked", 37),
            ("test_area", 46),
            ("test_area[1.0]", 43),
            ("test_area[1e-05]", 44),
            // As real pytest (9.2) collects them.
            ("test_values", 61),
            ("test_values[1e+16]", 51),
            ("test_values[-0.0]", 52),
            ("test_values[31]", 53),
            ("test_values[-7]", 54),
            ("test_values[a b]", 55),
            ("test_values[raw]", 56),
            ("test_values[v6]", 57),
            ("test_values[True]", 58),
            ("test_values[None]", 59),
        ]
        .iter()
        .map(|(n, l)| (n.to_string(), *l))
        .collect();
        assert_eq!(tests, expected);
        // From a case's line, that case; from the def, the whole test.
        let case = runnable_at(&found, 8).unwrap();
        assert_eq!(pytest_node("t.py", case), "t.py::test_add[big]");
        assert_eq!(runnable_at(&found, 14).unwrap().name, "test_add");
        assert_eq!(runnable_at(&found, 18).unwrap().name, "test_one_line");
        let area = runnable_at(&found, 44).unwrap();
        assert_eq!(
            pytest_node("t.py", area),
            "t.py::TestShapes::test_area[1e-05]"
        );
        // A module parametrizing all its tests: no case is known alone.
        let marked = format!("pytestmark = pytest.mark.parametrize(\"k\", [1, 2])\n{source}");
        assert!(
            runnables("python", &marked)
                .iter()
                .all(|r| !r.name.contains('[')),
        );
    }
}
