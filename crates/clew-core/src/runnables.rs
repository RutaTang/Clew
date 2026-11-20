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

/// What a runnable is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnableKind {
    /// A test: run on its own, by its test runner.
    Test,
    /// The program's start: Rust's `fn main`, Python's
    /// `if __name__ == "__main__":` block.
    Main,
}

/// Something in a file that can be run on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runnable {
    pub kind: RunnableKind,
    /// Its function's name (`adds`, `main`); `__main__` for Python's block.
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

/// The runnables of `source`, in language `lang` (a key), in order.
/// Rust and Python have them; other languages none yet.
pub fn runnables(lang: &str, source: &str) -> Vec<Runnable> {
    let language = match lang {
        "rust" => Lang::Rust,
        "python" => Lang::Python,
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
        Lang::Rust => rust(tree.root_node(), source, &entries, &mut scopes, &mut out),
        _ => python(tree.root_node(), source, &entries, &mut scopes, &mut out),
    }
    out
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
            "mod_item" if scopes.len() < MAX_SCOPES => {
                let (Some(name), Some(body)) = (
                    item.child_by_field_name("name"),
                    item.child_by_field_name("body"),
                ) else {
                    continue;
                };
                scopes.push(text(name, source).to_string());
                rust(body, source, entries, scopes, out);
                scopes.pop();
            }
            _ => {}
        }
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
                scopes.push(name_text.to_string());
                python(body, source, entries, scopes, out);
                scopes.pop();
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
    runnables
        .iter()
        .filter(|r| r.span.0 <= line && line <= r.span.1)
        .min_by_key(|r| r.span.1 - r.span.0)
}

/// A Python test's pytest node id: its file as given (`tests/test_x.py`,
/// slashes), then its classes and its name, `::` between.
pub fn pytest_node(rel: &str, runnable: &Runnable) -> String {
    let mut node = rel.replace('\\', "/");
    for scope in &runnable.scopes {
        node.push_str("::");
        node.push_str(scope);
    }
    node.push_str("::");
    node.push_str(&runnable.name);
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
                (RunnableKind::Main, "main".into(), 1, vec![]),
                (
                    RunnableKind::Test,
                    "adds".into(),
                    12,
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
        assert_eq!(runnable_at(&found, 5), None, "a plain function");
        assert_eq!(runnable_at(&found, 21), None, "in the module, no test");
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
                (RunnableKind::Test, "test_adds".into(), 5, vec![]),
                (RunnableKind::Test, "test_slow".into(), 10, vec![]),
                (
                    RunnableKind::Test,
                    "test_total".into(),
                    19,
                    vec!["TestPrices".to_string()]
                ),
                (
                    RunnableKind::Test,
                    "test_empty".into(),
                    27,
                    vec!["Checks".to_string()]
                ),
                (RunnableKind::Main, "__main__".into(), 36, vec![]),
            ]
        );
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
}
