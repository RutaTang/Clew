//! Weak spots: where a reader's attention is best spent first in a project
//! — long functions, deeply nested or branchy ones, ones that can panic,
//! run text as code or swallow errors, and notes left in the code saying
//! something is wrong. Each is measured on the file's syntax tree (comments
//! and strings set aside), scored, and ranked; how often the file changes
//! is weighed in by the caller ([`rank`]), as code that is both tangled and
//! often changed is where bugs collect.
//!
//! A heuristic to read by, not a bug report: nothing here claims a defect.

use tree_sitter::Node;

use crate::highlight::{self, Lang};
use crate::outline;

/// One place worth a look, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spot {
    /// Project-relative path of its file.
    pub rel: String,
    /// The function's name; empty for notes in a file outside any function.
    pub name: String,
    /// 1-based first line.
    pub line: usize,
    /// 1-based last line.
    pub end_line: usize,
    /// How much it asks for attention: the reasons' weights summed.
    pub score: u32,
    pub reasons: Vec<Reason>,
}

/// Why a spot asks for attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Its length, in lines, past [`LONG_LINES`].
    Long(usize),
    /// How deep its control flow nests, at least [`DEEP_NESTING`].
    Deep(usize),
    /// How many ways through it there are (branches, loops, arms,
    /// short-circuits), past [`MANY_BRANCHES`].
    Branchy(usize),
    /// A call or construct that can fail loudly or do harm, and how many.
    Risky(Risk, usize),
    /// Errors caught and dropped: an empty `catch`, `except: pass`.
    Swallowed(usize),
    /// `TODO`, `FIXME`, `HACK`, `XXX` or `BUG` notes in it.
    Marked(usize),
    /// Commits that changed its file (weighed in by [`rank`]).
    Churn(u32),
}

/// A kind of risky call or construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Risk {
    /// `unwrap()` / `expect()`: a panic where a value is missing.
    Unwrap,
    /// `panic!`, `unreachable!`, `todo!`, `unimplemented!`, Go's `panic(`.
    Panic,
    /// `unsafe` Rust.
    Unsafe,
    /// Text run as code: `eval`, `exec`, `new Function`.
    Eval,
    /// A shell command run: `os.system`, `shell=True`, `system(`.
    Shell,
    /// C string calls with no bound: `strcpy`, `strcat`, `sprintf`, `gets`.
    UnboundedCopy,
    /// Data turned back into objects: `pickle.load`, `unserialize`,
    /// `Marshal.load`.
    Deserialize,
    /// HTML written from strings: `innerHTML =`, `dangerouslySetInnerHTML`.
    RawHtml,
    /// Every exception caught: Python's bare `except:`.
    CatchAll,
}

impl Risk {
    /// How much one of it weighs.
    fn weight(self) -> u32 {
        match self {
            Risk::Unwrap => 1,
            Risk::Panic | Risk::CatchAll => 2,
            Risk::Unsafe | Risk::RawHtml => 3,
            Risk::Eval | Risk::Shell | Risk::UnboundedCopy | Risk::Deserialize => 4,
        }
    }
}

/// A function longer than this many lines is long.
pub const LONG_LINES: usize = 60;
/// Control flow nested this deep is deep.
pub const DEEP_NESTING: usize = 4;
/// More ways through a function than this is many.
pub const MANY_BRANCHES: usize = 12;
/// At most this many of one kind count toward a score: a file of a
/// hundred `unwrap()`s is not a hundred times worse than one of ten.
const COUNT_CAP: usize = 10;

/// The patterns that mark a risk in `lang`'s code, each with its kind.
/// A pattern starting with a letter must not follow an identifier
/// character (`myeval(` is no `eval(`).
fn risks(lang: Lang) -> &'static [(&'static str, Risk)] {
    match lang {
        Lang::Rust => &[
            (".unwrap()", Risk::Unwrap),
            (".expect(", Risk::Unwrap),
            ("panic!(", Risk::Panic),
            ("unreachable!(", Risk::Panic),
            ("todo!(", Risk::Panic),
            ("unimplemented!(", Risk::Panic),
            ("unsafe ", Risk::Unsafe),
        ],
        Lang::Python => &[
            ("eval(", Risk::Eval),
            ("exec(", Risk::Eval),
            ("os.system(", Risk::Shell),
            ("shell=True", Risk::Shell),
            ("pickle.load", Risk::Deserialize),
            ("except:", Risk::CatchAll),
        ],
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => &[
            ("eval(", Risk::Eval),
            ("new Function(", Risk::Eval),
            (".innerHTML =", Risk::RawHtml),
            ("dangerouslySetInnerHTML", Risk::RawHtml),
        ],
        Lang::Go => &[("panic(", Risk::Panic)],
        Lang::C | Lang::Cpp => &[
            ("strcpy(", Risk::UnboundedCopy),
            ("strcat(", Risk::UnboundedCopy),
            ("sprintf(", Risk::UnboundedCopy),
            ("gets(", Risk::UnboundedCopy),
            ("system(", Risk::Shell),
        ],
        Lang::Php => &[
            ("eval(", Risk::Eval),
            ("shell_exec(", Risk::Shell),
            ("passthru(", Risk::Shell),
            ("system(", Risk::Shell),
            ("unserialize(", Risk::Deserialize),
        ],
        Lang::Ruby => &[
            ("eval(", Risk::Eval),
            ("system(", Risk::Shell),
            ("Marshal.load", Risk::Deserialize),
        ],
        Lang::Java => &[("Runtime.getRuntime().exec(", Risk::Shell)],
        _ => &[],
    }
}

/// Whether a path is a test's: its findings are not the project's weak
/// spots (tests unwrap and panic by design).
pub fn is_test_path(rel: &str) -> bool {
    let rel = rel.replace('\\', "/");
    let name = rel.rsplit('/').next().unwrap_or(&rel);
    rel.split('/').any(|part| {
        matches!(
            part,
            "tests" | "test" | "__tests__" | "spec" | "testdata" | "fixtures" | "benches"
        )
    }) || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.ends_with("_tests.rs")
        || name == "tests.rs"
        || name == "conftest.py"
}

/// The weak spots of one file: each function that asks for attention, and
/// notes outside any function as one spot of the file. `lang_key` is the
/// file's language ([`highlight::detect`]). Test files and test functions
/// have none.
pub fn scan_file(rel: &str, source: &str, lang_key: &str) -> Vec<Spot> {
    if is_test_path(rel) {
        return Vec::new();
    }
    let Some(lang) = Lang::for_source(lang_key, source) else {
        return Vec::new();
    };
    if lang == Lang::Lean {
        return Vec::new();
    }
    let Some(tree) = highlight::parse(source, lang) else {
        return Vec::new();
    };
    let lines: Vec<&str> = source.lines().collect();
    let tests = outline::EntryPoints::new(&lines, lang_key, rel);
    let located = outline::located_in(&tree, source, lang);
    let mut spots = Vec::new();
    // The byte ranges of the functions measured: a note outside all of
    // them is the file's.
    let mut covered: Vec<(usize, usize)> = Vec::new();
    for item in &located {
        let symbol = &item.symbol;
        if !outline::is_callable(&symbol.kind) || item.body.is_none() {
            continue;
        }
        if tests.is_test(symbol.line, &symbol.name) {
            covered.push(item.declaration);
            continue;
        }
        let (start, end) = item.declaration;
        let Some(node) = tree
            .root_node()
            .descendant_for_byte_range(start, end.saturating_sub(1).max(start))
        else {
            continue;
        };
        covered.push(item.declaration);
        let (first, last) = item.body.unwrap_or((symbol.line, symbol.end_line));
        let measured = measure(node, source, lang);
        let length = last.saturating_sub(item.decl_line.min(first)) + 1;
        let (score, reasons) = judge(length, &measured);
        if score > 0 {
            spots.push(Spot {
                rel: rel.to_string(),
                name: symbol.name.clone(),
                line: item.decl_line.min(first).max(1),
                end_line: last,
                score,
                reasons,
            });
        }
    }
    // Notes outside every function: the file's own.
    let mut loose = Vec::new();
    comments(tree.root_node(), &mut |node| {
        let at = node.start_byte();
        if covered.iter().any(|&(s, e)| s <= at && at < e) {
            return;
        }
        let n = markers(&source[node.byte_range()]);
        if n > 0 {
            loose.push((node.start_position().row + 1, n));
        }
    });
    if let Some(&(line, _)) = loose.first() {
        let count: usize = loose.iter().map(|(_, n)| n).sum();
        let (score, reasons) = judge(
            0,
            &Measured {
                markers: count,
                ..Measured::default()
            },
        );
        spots.push(Spot {
            rel: rel.to_string(),
            name: String::new(),
            line,
            end_line: loose.last().map_or(line, |&(l, _)| l),
            score,
            reasons,
        });
    }
    spots
}

/// What a function's syntax says of it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Measured {
    depth: usize,
    branches: usize,
    risks: Vec<(Risk, usize)>,
    swallowed: usize,
    markers: usize,
}

/// A function's score and its reasons, from its length in lines and what
/// was measured in it.
fn judge(length: usize, m: &Measured) -> (u32, Vec<Reason>) {
    let mut score = 0u32;
    let mut reasons = Vec::new();
    if length > LONG_LINES {
        score += 2 + ((length - LONG_LINES) / 15) as u32;
        reasons.push(Reason::Long(length));
    }
    if m.depth >= DEEP_NESTING {
        score += 3 * (m.depth + 1 - DEEP_NESTING) as u32;
        reasons.push(Reason::Deep(m.depth));
    }
    if m.branches > MANY_BRANCHES {
        score += 1 + ((m.branches - MANY_BRANCHES) / 3) as u32;
        reasons.push(Reason::Branchy(m.branches));
    }
    for &(risk, count) in &m.risks {
        score += risk.weight() * count.min(COUNT_CAP) as u32;
        reasons.push(Reason::Risky(risk, count));
    }
    if m.swallowed > 0 {
        score += 4 * m.swallowed.min(COUNT_CAP) as u32;
        reasons.push(Reason::Swallowed(m.swallowed));
    }
    if m.markers > 0 {
        score += 3 * m.markers.min(COUNT_CAP) as u32;
        reasons.push(Reason::Marked(m.markers));
    }
    (score, reasons)
}

/// Measure the function at `node`: how deep its control flow nests, how
/// many ways through it there are, its risky calls (in code, not comments
/// or strings), the errors it swallows and the notes left in it.
fn measure(node: Node, source: &str, lang: Lang) -> Measured {
    let mut m = Measured::default();
    walk(node, 0, &mut m, source);
    // The function's text with comments and strings blanked, so that a
    // pattern in either is not counted.
    let range = node.byte_range();
    let mut code: Vec<u8> = source.as_bytes()[range.clone()].to_vec();
    blank(node, range.start, &mut code);
    let code = String::from_utf8_lossy(&code);
    let mut counts: Vec<(Risk, usize)> = Vec::new();
    for &(pattern, risk) in risks(lang) {
        let n = occurrences(&code, pattern);
        if n == 0 {
            continue;
        }
        match counts.iter_mut().find(|(r, _)| *r == risk) {
            Some((_, c)) => *c += n,
            None => counts.push((risk, n)),
        }
    }
    m.risks = counts;
    comments(node, &mut |c| m.markers += markers(&source[c.byte_range()]));
    m
}

/// Count `node`'s decisions and track the deepest nesting of its control
/// structures, `depth` deep already.
fn walk(node: Node, depth: usize, m: &mut Measured, source: &str) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        let mut inner = depth;
        if let Some(nests) = decision(kind) {
            m.branches += 1;
            // `else if` goes on the same level as its `if`.
            let chained = is_if(kind)
                && child
                    .parent()
                    .is_some_and(|p| p.kind().contains("else") || is_if(p.kind()));
            if nests && !chained {
                inner = depth + 1;
                m.depth = m.depth.max(inner);
            }
        } else if short_circuit(child, source) {
            m.branches += 1;
        }
        if is_catch(kind) && swallows(child) {
            m.swallowed += 1;
        }
        // A function inside is measured as its own, not as part of this
        // one's nesting: its body still counts toward this one's branches.
        walk(child, inner, m, source);
    }
}

/// Whether a node of `kind` is a decision: `Some(true)` for a structure
/// whose body nests (an `if`, a loop, a `match`), `Some(false)` for one
/// more way through that does not (an arm, a `catch`, a ternary).
fn decision(kind: &str) -> Option<bool> {
    const NESTS: &[&str] = &[
        "if_statement",
        "if_expression",
        "if_let_expression",
        "for_statement",
        "for_expression",
        "for_in_statement",
        "for_of_statement",
        "enhanced_for_statement",
        "foreach_statement",
        "while_statement",
        "while_expression",
        "while_let_expression",
        "do_statement",
        "loop_expression",
        "match_expression",
        "match_statement",
        "switch_statement",
        "switch_expression",
        "expression_switch_statement",
        "type_switch_statement",
        "select_statement",
        "try_statement",
        "try_with_resources_statement",
        "if",
        "unless",
        "while",
        "until",
        "for",
        "case",
    ];
    const BRANCHES: &[&str] = &[
        "match_arm",
        "case_clause",
        "switch_case",
        "switch_section",
        "switch_block_statement_group",
        "expression_case",
        "type_case",
        "communication_case",
        "catch_clause",
        "except_clause",
        "elif_clause",
        "conditional_expression",
        "ternary_expression",
        "when",
        "rescue",
    ];
    if NESTS.contains(&kind) {
        Some(true)
    } else if BRANCHES.contains(&kind) {
        Some(false)
    } else {
        None
    }
}

fn is_if(kind: &str) -> bool {
    matches!(
        kind,
        "if_statement" | "if_expression" | "if_let_expression" | "if" | "unless"
    )
}

fn is_catch(kind: &str) -> bool {
    matches!(kind, "catch_clause" | "except_clause" | "rescue")
}

/// Whether `node` is `&&` / `||` (`and` / `or` in Python): one more way
/// through.
fn short_circuit(node: Node, source: &str) -> bool {
    match node.kind() {
        "boolean_operator" => true,
        "binary_expression" => node
            .child_by_field_name("operator")
            .and_then(|op| source.get(op.byte_range()))
            .is_some_and(|op| op == "&&" || op == "||"),
        _ => false,
    }
}

/// Whether a `catch` / `except` / `rescue` drops what it caught: its body
/// holds nothing but `pass` and comments.
fn swallows(clause: Node) -> bool {
    let body = clause.child_by_field_name("body").or_else(|| {
        let mut cursor = clause.walk();
        clause
            .named_children(&mut cursor)
            .filter(|c| matches!(c.kind(), "block" | "statement_block" | "body_statement"))
            .last()
    });
    let Some(body) = body else {
        // Ruby's `rescue` with no statements has no body node at all.
        return clause.kind() == "rescue" && clause.named_child_count() <= 2;
    };
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .all(|s| s.kind().contains("comment") || s.kind() == "pass_statement")
}

/// Call `each` on every comment node under `node`.
fn comments(node: Node, each: &mut impl FnMut(Node)) {
    if node.kind().contains("comment") {
        each(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        comments(child, each);
    }
}

/// Blank out, in `code` (the source from byte `base`), every comment and
/// string under `node`.
fn blank(node: Node, base: usize, code: &mut [u8]) {
    let kind = node.kind();
    if kind.contains("comment") || kind.contains("string") || kind == "char_literal" {
        for b in &mut code[node.start_byte() - base..node.end_byte() - base] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        blank(child, base, code);
    }
}

/// Times `pattern` occurs in `code` as a whole token: one starting with an
/// identifier character must not follow one.
fn occurrences(code: &str, pattern: &str) -> usize {
    let word_start = pattern
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_');
    code.match_indices(pattern)
        .filter(|&(at, _)| {
            !word_start
                || !code[..at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
        })
        .count()
}

/// How many `TODO`, `FIXME`, `HACK`, `XXX` or `BUG` notes a comment holds.
fn markers(comment: &str) -> usize {
    const WORDS: [&str; 5] = ["TODO", "FIXME", "HACK", "XXX", "BUG"];
    let word = |c: char| c.is_alphanumeric() || c == '_';
    WORDS
        .iter()
        .map(|w| {
            comment
                .match_indices(w)
                .filter(|&(at, _)| {
                    !comment[..at].chars().next_back().is_some_and(word)
                        && !comment[at + w.len()..].chars().next().is_some_and(word)
                })
                .count()
        })
        .sum()
}

/// The spots of many files, ranked: each file's churn (`churn(rel)`, the
/// commits that changed it) weighed in — a tangled function in a file that
/// changes often is where bugs collect — and the `limit` that score
/// highest kept, highest first (ties by path, then line).
pub fn rank(mut spots: Vec<Spot>, churn: impl Fn(&str) -> Option<u32>, limit: usize) -> Vec<Spot> {
    for spot in &mut spots {
        if let Some(commits) = churn(&spot.rel).filter(|&c| c >= 3) {
            spot.score += spot.score * commits.min(30) / 15;
            spot.reasons.push(Reason::Churn(commits));
        }
    }
    spots.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.rel.cmp(&b.rel))
            .then_with(|| a.line.cmp(&b.line))
    });
    spots.truncate(limit);
    spots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reasons(spots: &[Spot], name: &str) -> Vec<Reason> {
        spots
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.reasons.clone())
            .unwrap_or_default()
    }

    #[test]
    fn rust_panics_unsafe_and_nesting_are_found_in_code_only() {
        let src = r#"
fn load(path: &str) -> Config {
    // TODO: report a missing file instead of panicking
    let text = std::fs::read_to_string(path).unwrap();
    let s = "this .unwrap() is in a string";
    if a {
        for x in xs {
            while b {
                match c {
                    1 => if d { e(); } else if f { g(); },
                    _ => {}
                }
            }
        }
    }
    unsafe { std::mem::transmute(text) }
}

fn fine(a: u32) -> u32 {
    a + 1
}

#[test]
fn a_test() {
    let v: Option<u8> = None;
    v.unwrap();
}
"#;
        let spots = scan_file("src/config.rs", src, "rust");
        let load = reasons(&spots, "load");
        assert!(load.contains(&Reason::Risky(Risk::Unwrap, 1)), "{load:?}");
        assert!(load.contains(&Reason::Risky(Risk::Unsafe, 1)), "{load:?}");
        assert!(load.contains(&Reason::Marked(1)), "{load:?}");
        // if > for > while > match > if: five deep, the else-if on its
        // if's level.
        assert!(load.contains(&Reason::Deep(5)), "{load:?}");
        assert!(reasons(&spots, "fine").is_empty(), "nothing to say of it");
        assert!(
            spots.iter().all(|s| s.name != "a_test"),
            "a test unwraps by design"
        );
        assert!(scan_file("tests/config.rs", src, "rust").is_empty());
    }

    #[test]
    fn python_swallowed_errors_eval_and_branches() {
        let mut src =
            String::from("def risky(x):\n    try:\n        eval(x)\n    except:\n        pass\n");
        for i in 0..14 {
            src.push_str(&format!("    if x == {i} and x:\n        return {i}\n"));
        }
        src.push_str("    return None\n\ndef myeval(x):\n    return x\n");
        let spots = scan_file("app/run.py", &src, "python");
        let risky = reasons(&spots, "risky");
        assert!(risky.contains(&Reason::Swallowed(1)), "{risky:?}");
        assert!(risky.contains(&Reason::Risky(Risk::Eval, 1)), "{risky:?}");
        assert!(
            risky.contains(&Reason::Risky(Risk::CatchAll, 1)),
            "{risky:?}"
        );
        // 14 ifs, 14 `and`s, the try and its except.
        assert!(risky.contains(&Reason::Branchy(30)), "{risky:?}");
        assert!(reasons(&spots, "myeval").is_empty(), "a name is no call");
    }

    #[test]
    fn a_long_function_and_a_files_loose_notes() {
        let mut src = String::from("// FIXME: this module leaks\nfunction big() {\n");
        for i in 0..70 {
            src.push_str(&format!("  console.log({i});\n"));
        }
        src.push_str("}\n");
        let spots = scan_file("web/big.js", &src, "javascript");
        assert!(
            reasons(&spots, "big").contains(&Reason::Long(72)),
            "{spots:?}"
        );
        let file = spots
            .iter()
            .find(|s| s.name.is_empty())
            .expect("the file's note");
        assert_eq!(file.line, 1);
        assert_eq!(file.reasons, [Reason::Marked(1)]);
        // TODOLIST is no TODO.
        assert_eq!(markers("// TODOLIST, XXXL, BUGS"), 0);
        assert_eq!(markers("// TODO(ann): x; FIXME"), 2);
    }

    #[test]
    fn churn_raises_a_spot_and_the_list_is_cut_to_the_highest() {
        let spot = |rel: &str, score| Spot {
            rel: rel.into(),
            name: "f".into(),
            line: 1,
            end_line: 2,
            score,
            reasons: Vec::new(),
        };
        let ranked = rank(
            vec![spot("a.rs", 10), spot("b.rs", 8), spot("c.rs", 1)],
            |rel| (rel == "b.rs").then_some(30),
            2,
        );
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].rel, "b.rs", "8 doubled by its churn");
        assert_eq!(ranked[0].score, 24);
        assert_eq!(ranked[0].reasons, [Reason::Churn(30)]);
        assert_eq!(ranked[1].rel, "a.rs");
        // A file changed twice is not churn.
        let calm = rank(vec![spot("a.rs", 10)], |_| Some(2), 5);
        assert!(calm[0].reasons.is_empty());
    }

    #[test]
    fn test_paths_are_known() {
        for rel in [
            "tests/a.rs",
            "src/foo_test.go",
            "pkg/test_util.py",
            "web/app.test.ts",
            "web/__tests__/x.js",
            "src/app/tests.rs",
        ] {
            assert!(is_test_path(rel), "{rel}");
        }
        for rel in ["src/testing_tools.rs", "src/attest.rs", "contest/main.go"] {
            assert!(!is_test_path(rel), "{rel}");
        }
    }
}
