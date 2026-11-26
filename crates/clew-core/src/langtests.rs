//! Tests at the cursor for the languages whose runner is a build tool or a
//! test command of their own — Go, Java, Kotlin, Scala, C#, Dart, Ruby,
//! Zig, Erlang, Elixir, Haskell, C and C++: which tests a file has, each by
//! the name its runner knows it by, and the command that runs one, as the
//! project runs its tests (`go test`, Gradle or Maven, sbt, Mill or
//! scala-cli, `dotnet test`, `dart test` or `flutter test`, RSpec or
//! Minitest, `zig test`, rebar3, `mix test`, Cabal or Stack, CTest).
//!
//! Each test is found by its language's own convention, read off the
//! source: a Go `func TestX(t *testing.T)`, a JUnit `@Test` method, a
//! `[Fact]`, a `test('…')` call, an `it "…"` block, a `test "…" {}`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::jsrun::JsCommand;
use crate::runnables::{Runnable, RunnableKind};

/// The languages whose tests are found here.
pub fn handles(lang: &str) -> bool {
    matches!(
        lang,
        "go" | "java"
            | "kotlin"
            | "scala"
            | "csharp"
            | "dart"
            | "ruby"
            | "zig"
            | "erlang"
            | "elixir"
            | "haskell"
            | "c"
            | "cpp"
    )
}

/// The tests of `source`, a file in `lang` at `rel` (its name tells a test
/// file in some languages: `_test.go`, `_spec.rb`, `_SUITE.erl`). A file
/// with any has its every test as a group of its own, first.
pub fn tests(lang: &str, rel: &str, source: &str) -> Vec<Runnable> {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = match lang {
        "go" if rel.ends_with("_test.go") => go_tests(&lines),
        "java" | "kotlin" | "scala" => {
            let mut found = junit_tests(lang, source, &lines);
            if lang == "scala" && found.is_empty() {
                found = called_tests(&lines, &["test", "it", "property"], &["describe"]);
                found = with_suite_classes(&lines, found);
            }
            found
        }
        "csharp" => csharp_tests(source, &lines),
        "dart" => called_tests(&lines, &["test", "testWidgets"], &["group"]),
        "ruby" if rel.ends_with("_spec.rb") => called_tests(
            &lines,
            &["it", "specify", "scenario"],
            &["describe", "context", "feature"],
        ),
        "ruby" => minitest_tests(&lines),
        "zig" => zig_tests(&lines),
        "erlang" => erlang_tests(rel, &lines),
        "elixir" => called_tests(&lines, &["test"], &["describe"]),
        "haskell" => {
            // hspec's `spec = describe "Cart" $ do`: the group a binding
            // opens, read as the call it is bound to.
            let unbound: Vec<String> = lines.iter().map(|l| haskell_unbound(l)).collect();
            let unbound: Vec<&str> = unbound.iter().map(String::as_str).collect();
            called_tests(
                &unbound,
                &["it", "prop", "specify", "testCase", "testProperty"],
                &["describe", "context", "testGroup"],
            )
        }
        "c" | "cpp" => cpp_tests(&lines),
        _ => Vec::new(),
    };
    if !out.is_empty() {
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
    // A Dart test file's `main` is its tests' (`group`, `test` inside it):
    // they are what its ▶s run.
    let tests_main = lang == "dart" && !out.is_empty();
    if let Some(line) = main_line(lang, &lines).filter(|_| !tests_main) {
        out.push(Runnable {
            kind: RunnableKind::Main,
            name: "main".to_string(),
            line,
            // Its own line: elsewhere in it, the cursor is where it was —
            // in the file's tests, for Run Test.
            span: (line, line),
            scopes: Vec::new(),
        });
    }
    out
}

/// The 1-based line of a C# file's first top-level statement: a program
/// with no `Main`, its statements before any type or namespace.
fn csharp_top_level(lines: &[&str]) -> Option<usize> {
    let mut in_comment = false;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if in_comment {
            in_comment = !t.contains("*/");
            continue;
        }
        if t.is_empty() || t.starts_with("//") || t.starts_with('#') {
            continue;
        }
        if t.starts_with("/*") {
            in_comment = !t.contains("*/");
            continue;
        }
        if t.starts_with("using ") || t.starts_with("global using ") {
            continue;
        }
        let first = t
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .next()
            .unwrap_or("");
        let declares = t.starts_with('[')
            || matches!(
                first,
                "namespace"
                    | "class"
                    | "record"
                    | "struct"
                    | "interface"
                    | "enum"
                    | "delegate"
                    | "public"
                    | "internal"
                    | "private"
                    | "protected"
                    | "static"
                    | "sealed"
                    | "abstract"
                    | "partial"
                    | "file"
                    | "readonly"
                    | "unsafe"
            );
        return (!declares).then_some(i + 1);
    }
    None
}

/// The 1-based line a program of `lang` starts at in a file, as its
/// language starts one: Go's `func main()` of `package main`, C's and
/// C++'s `int main(`, Java's `static void main(` (or a compact source
/// file's `void main()`), Kotlin's top-level `fun main(`, Scala's `def
/// main(args` or `@main def`, C#'s `static … Main(`, Dart's top-level
/// `main(`, Zig's `pub fn main(`, Haskell's `main` of its `Main` module,
/// an Erlang module's exported `main/1` (an escript's, the entry Clew's
/// Erlang debugger runs), and Ruby's `if __FILE__ == $0` block.
pub fn main_line(lang: &str, lines: &[&str]) -> Option<usize> {
    let code = |l: &&str| {
        let t = l.trim_start();
        !(t.starts_with("//") || t.starts_with('*') || t.starts_with("/*"))
    };
    let at = |pick: &dyn Fn(&str, &str) -> bool| {
        lines
            .iter()
            .position(|l| code(l) && pick(l, l.trim()))
            .map(|i| i + 1)
    };
    let top = |l: &str| !l.starts_with([' ', '\t']);
    match lang {
        "go" => lines
            .iter()
            .any(|l| l.trim() == "package main")
            .then(|| at(&|l, _| l.starts_with("func main()")))
            .flatten(),
        "c" | "cpp" => at(&|l, t| {
            top(l)
                && [
                    "int main(",
                    "int main (",
                    "int\tmain(",
                    "auto main(",
                    "void main(",
                    "signed main(",
                ]
                .iter()
                .any(|p| t.starts_with(p))
        }),
        "java" => {
            at(&|l, t| t.contains("static void main(") || (top(l) && t.starts_with("void main(")))
        }
        // Top level, or an object's `@JvmStatic` one (on its line or the
        // one above).
        "kotlin" => {
            let jvm_static = |i: usize| {
                lines[..i]
                    .iter()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .is_some_and(|l| l.trim() == "@JvmStatic")
            };
            lines
                .iter()
                .enumerate()
                .position(|(i, l)| {
                    let t = l.trim();
                    let main = t.starts_with("fun main(") || t.starts_with("suspend fun main(");
                    (main && (top(l) || jvm_static(i))) || t.starts_with("@JvmStatic fun main(")
                })
                .map(|i| i + 1)
        }
        // Scala 2's `object Main extends App` too.
        "scala" => at(&|_, t| {
            t.starts_with("def main(args")
                || t.starts_with("@main def ")
                || t.starts_with("@main\tdef ")
                || (t.starts_with("object ")
                    && (t.contains(" extends App ")
                        || t.ends_with(" extends App")
                        || t.contains(" extends App{")))
        }),
        // A `Main`, or a program's top-level statements (what `dotnet new
        // console` writes): the first, before any declaration.
        "csharp" => at(&|_, t| {
            t.contains("static ")
                && t.contains(" Main(")
                && (t.contains(" void ") || t.contains(" int ") || t.contains(" Task"))
        })
        .or_else(|| csharp_top_level(lines)),
        "dart" => at(&|l, t| {
            top(l)
                && [
                    "void main(",
                    "main(",
                    "Future<void> main(",
                    "Future main(",
                    "int main(",
                ]
                .iter()
                .any(|p| t.starts_with(p))
        }),
        "zig" => at(&|l, t| top(l) && t.starts_with("pub fn main(")),
        "haskell" => {
            let module = lines
                .iter()
                .find_map(|l| l.strip_prefix("module "))
                .map(|m| m.split_whitespace().next().unwrap_or(""));
            matches!(module, None | Some("Main"))
                .then(|| {
                    at(&|l, _| l.starts_with("main ::") || l.starts_with("main =") || l == "main")
                })
                .flatten()
        }
        "erlang" => {
            let escript = lines.first().is_some_and(|l| l.starts_with("#!"));
            let exported = lines.iter().any(|l| {
                let t = l.trim_start();
                t.starts_with("-export(") && t.replace(' ', "").contains("main/1")
            });
            (escript || exported)
                .then(|| at(&|l, _| l.starts_with("main(")))
                .flatten()
        }
        "ruby" => at(&|_, t| {
            t.starts_with("if __FILE__ == $0")
                || t.starts_with("if $0 == __FILE__")
                || t.starts_with("if __FILE__ == $PROGRAM_NAME")
                || t.starts_with("if $PROGRAM_NAME == __FILE__")
        }),
        _ => None,
    }
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The last line of what opens at `start` (0-based): the lines after it
/// indented deeper, and the closing line at its own indent (`}`, `end`,
/// `})`) — or, for a one-line test, the line itself.
fn span_from(lines: &[&str], start: usize) -> usize {
    let own = indent(lines[start]);
    let mut last = start;
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if indent(line) > own {
            last = i;
            continue;
        }
        if indent(line) == own && is_closer(t) {
            last = i;
        }
        break;
    }
    last
}

fn is_closer(t: &str) -> bool {
    t.starts_with('}') || t == "end" || t.starts_with("end)") || t.starts_with("})") || t == ")"
}

/// The first quoted string on `rest` (`"…"` or `'…'`), unescaped quotes
/// ending it.
fn quoted(rest: &str) -> Option<String> {
    let rest = rest.trim_start();
    let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let body = &rest[1..];
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            c if c == quote => return Some(out),
            c => out.push(c),
        }
    }
    None
}

/// Tests that are calls naming them, as Dart's, RSpec's, ExUnit's, hspec's
/// and ScalaTest's are: `test("adds") {`, `it "adds" do`, `test "adds" do`,
/// each in the groups (`describe "Cart"`) around it.
/// A Scala file's suites (MUnit's, ScalaTest's) as groups of their own:
/// each class around its `test(…)` calls, run as a whole from its line.
fn with_suite_classes(lines: &[&str], tests: Vec<Runnable>) -> Vec<Runnable> {
    let mut classes: Vec<Runnable> = Vec::new();
    for test in &tests {
        let Some(at) = (0..test.line.saturating_sub(1)).rev().find(|&i| {
            indent(lines[i]) < indent(lines[test.line - 1]) && class_name(lines[i]).is_some()
        }) else {
            continue;
        };
        if classes.iter().any(|c| c.line == at + 1) {
            continue;
        }
        classes.push(Runnable {
            kind: RunnableKind::Tests,
            name: class_name(lines[at]).unwrap_or_default(),
            line: at + 1,
            span: (at + 1, span_from(lines, at) + 1),
            scopes: Vec::new(),
        });
    }
    let mut all: Vec<Runnable> = classes.into_iter().chain(tests).collect();
    all.sort_by_key(|r| r.line);
    all
}

fn called_tests(lines: &[&str], tests: &[&str], groups: &[&str]) -> Vec<Runnable> {
    let mut out = Vec::new();
    // The groups open around the line, by their indent.
    let mut open: Vec<(usize, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        let Some((word, rest)) = t.split_once(['(', ' ']) else {
            continue;
        };
        let is_test = tests.contains(&word);
        let is_group = groups.contains(&word);
        if !is_test && !is_group {
            continue;
        }
        let Some(name) = quoted(rest) else {
            continue;
        };
        let depth = indent(line);
        open.retain(|(at, _)| *at < depth);
        let end = span_from(lines, i);
        let scopes: Vec<String> = open.iter().map(|(_, n)| n.clone()).collect();
        out.push(Runnable {
            kind: if is_test {
                RunnableKind::Test
            } else {
                RunnableKind::Tests
            },
            name: name.clone(),
            line: i + 1,
            span: (i + 1, end + 1),
            scopes,
        });
        if is_group {
            open.push((depth, name));
        }
    }
    out
}

/// A Haskell line without the binding it starts with: `spec = describe
/// "Cart" $ do` as `describe "Cart" $ do`, at the line's own indent.
fn haskell_unbound(line: &str) -> String {
    let at = indent(line);
    let bound = line[at..].split_once(" = ").filter(|(name, _)| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '\''))
    });
    match bound {
        Some((_, rest)) => [&line[..at], rest.trim_start()].concat(),
        None => line.to_string(),
    }
}

/// Go: `func TestX(t *testing.T)`, `BenchmarkX`, `FuzzX` and `ExampleX`,
/// at the top level of a `_test.go` file.
fn go_tests(lines: &[&str]) -> Vec<Runnable> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix("func ") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let named = ["Test", "Benchmark", "Fuzz", "Example"]
            .iter()
            .any(|p| name.starts_with(p));
        if !named || !rest[name.len()..].starts_with('(') {
            continue;
        }
        let end = span_from(lines, i);
        out.push(test(name, i, end, Vec::new()));
    }
    out
}

fn test(name: String, line: usize, end: usize, scopes: Vec<String>) -> Runnable {
    Runnable {
        kind: RunnableKind::Test,
        name,
        line: line + 1,
        span: (line + 1, end + 1),
        scopes,
    }
}

/// JUnit (Java, Kotlin, Scala) and TestNG: a method annotated `@Test`,
/// `@ParameterizedTest`, `@RepeatedTest`, `@TestFactory` or
/// `@TestTemplate`, in its class; the class is a group of its own.
fn junit_tests(lang: &str, source: &str, lines: &[&str]) -> Vec<Runnable> {
    const MARKS: &[&str] = &[
        "@Test",
        "@ParameterizedTest",
        "@RepeatedTest",
        "@TestFactory",
        "@TestTemplate",
        "@org.junit.Test",
        "@org.junit.jupiter.api.Test",
    ];
    let symbols = crate::outline::extract(source, lang);
    let classes: Vec<&crate::outline::Symbol> = symbols
        .iter()
        .filter(|s| matches!(s.kind.as_str(), "class" | "struct"))
        .collect();
    let class_of = |line: usize| {
        classes
            .iter()
            .filter(|c| c.line < line && line <= c.end_line)
            .max_by_key(|c| c.line)
    };
    let mut out = Vec::new();
    let mut tested_classes: Vec<&crate::outline::Symbol> = Vec::new();
    for s in symbols
        .iter()
        .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
    {
        // The annotations above it, and on its own line before its name.
        let mut i = s.line.saturating_sub(1);
        let mut head = lines.get(i).copied().unwrap_or("").to_string();
        while i > 0 {
            let above = lines[i - 1].trim();
            if above.starts_with('@') {
                head.push('\n');
                head.push_str(above);
                i -= 1;
            } else {
                break;
            }
        }
        let marked = MARKS.iter().any(|m| {
            head.split(|c: char| c.is_whitespace() || c == '(')
                .any(|w| w == *m)
        });
        if !marked {
            continue;
        }
        let Some(class) = class_of(s.line) else {
            continue;
        };
        if !tested_classes.iter().any(|c| c.line == class.line) {
            tested_classes.push(class);
        }
        // Its mark on the line naming it: the definition starts at its
        // annotations.
        let end = s.end_line.max(s.line) - 1;
        let named = (s.line - 1..=end)
            .find(|&i| lines.get(i).is_some_and(|l| l.contains(s.name.as_str())))
            .unwrap_or(s.line - 1);
        let name = s.name.trim_matches('`').to_string();
        out.push(test(name, named, end, vec![class.name.clone()]));
    }
    for class in tested_classes {
        out.push(Runnable {
            kind: RunnableKind::Tests,
            name: class.name.clone(),
            line: class.line,
            span: (class.line, class.end_line.max(class.line)),
            scopes: Vec::new(),
        });
    }
    out.sort_by_key(|r| r.line);
    out
}

/// C#: a method marked `[Fact]`, `[Theory]`, `[Test]`, `[TestCase(…)]`,
/// `[TestMethod]` or `[DataTestMethod]` (xUnit, NUnit, MSTest), in its
/// class.
fn csharp_tests(source: &str, lines: &[&str]) -> Vec<Runnable> {
    let symbols = crate::outline::extract(source, "csharp");
    let classes: Vec<&crate::outline::Symbol> = symbols
        .iter()
        .filter(|s| matches!(s.kind.as_str(), "class" | "struct"))
        .collect();
    let mut out = Vec::new();
    let mut tested: Vec<&crate::outline::Symbol> = Vec::new();
    for s in symbols.iter().filter(|s| s.kind == "method") {
        // Its attributes: the lines of the definition before the one
        // naming it (the definition starts at them), and any just above.
        let end = s.end_line.max(s.line) - 1;
        let named = (s.line - 1..=end)
            .find(|&i| {
                lines
                    .get(i)
                    .is_some_and(|l| l.contains(&format!("{}(", s.name)))
            })
            .unwrap_or(s.line - 1);
        let mut first = s.line - 1;
        while first > 0 && lines[first - 1].trim().starts_with('[') {
            first -= 1;
        }
        let marked = lines[first..named].iter().any(|line| {
            let line = line.trim();
            line.starts_with('[')
                && line
                    .trim_start_matches('[')
                    .split([',', ']'])
                    .map(|a| a.trim().split('(').next().unwrap_or("").trim())
                    .any(|a| {
                        matches!(
                            a,
                            "Fact"
                                | "Theory"
                                | "Test"
                                | "TestCase"
                                | "TestCaseSource"
                                | "TestMethod"
                                | "DataTestMethod"
                        )
                    })
        });
        let Some(class) = classes
            .iter()
            .filter(|c| c.line < s.line && s.line <= c.end_line)
            .max_by_key(|c| c.line)
        else {
            continue;
        };
        if !marked {
            continue;
        }
        if !tested.iter().any(|c| c.line == class.line) {
            tested.push(class);
        }
        out.push(test(s.name.clone(), named, end, vec![class.name.clone()]));
    }
    for class in tested {
        out.push(Runnable {
            kind: RunnableKind::Tests,
            name: class.name.clone(),
            line: class.line,
            span: (class.line, class.end_line.max(class.line)),
            scopes: Vec::new(),
        });
    }
    out.sort_by_key(|r| r.line);
    out
}

/// Minitest: `def test_x` methods, and Rails' `test "x" do` blocks (whose
/// method name is `test_x`, its spaces underscores).
fn minitest_tests(lines: &[&str]) -> Vec<Runnable> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        let name = if let Some(rest) = t.strip_prefix("def test_") {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '?' | '!'))
                .collect();
            format!("test_{name}")
        } else if let Some(rest) = t.strip_prefix("test ") {
            match quoted(rest) {
                Some(name) => format!("test_{}", name.replace(' ', "_")),
                None => continue,
            }
        } else {
            continue;
        };
        let end = span_from(lines, i);
        out.push(test(name, i, end, Vec::new()));
    }
    out
}

/// Zig: `test "name" {` and `test name {` (a declaration's test).
fn zig_tests(lines: &[&str]) -> Vec<Runnable> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.trim_start().strip_prefix("test ") else {
            continue;
        };
        let name = match quoted(rest) {
            Some(name) => name,
            None => rest.trim().trim_end_matches('{').trim().to_string(),
        };
        if name.is_empty() {
            continue;
        }
        let end = span_from(lines, i);
        out.push(test(name, i, end, Vec::new()));
    }
    out
}

/// Erlang: EUnit's `x_test()` functions and `x_test_()` generators, and a
/// Common Test suite's (`x_SUITE.erl`) cases — its functions of one
/// argument, less the suite's own callbacks.
fn erlang_tests(rel: &str, lines: &[&str]) -> Vec<Runnable> {
    let suite = rel.ends_with("_SUITE.erl");
    let mut out: Vec<Runnable> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with(char::is_whitespace) || line.starts_with('%') || line.starts_with('-') {
            continue;
        }
        let name: String = line
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let Some(args) = line[name.len()..].strip_prefix('(') else {
            continue;
        };
        let eunit = name.ends_with("_test") || name.ends_with("_test_");
        let one_arg = !args.trim_start().starts_with(')')
            && !args.split(')').next().unwrap_or("").contains(',');
        let callback = matches!(
            name.as_str(),
            "all"
                | "groups"
                | "suite"
                | "init_per_suite"
                | "end_per_suite"
                | "init_per_group"
                | "end_per_group"
                | "init_per_testcase"
                | "end_per_testcase"
        );
        let case = suite && one_arg && !callback;
        if !(eunit || case) || out.last().is_some_and(|r| r.name == name) {
            continue;
        }
        // To the clause's end: the line its `.` ends.
        let end = (i..lines.len())
            .find(|&j| lines[j].trim_end().ends_with('.'))
            .unwrap_or(i);
        out.push(test(name, i, end, Vec::new()));
    }
    out
}

/// C and C++: GoogleTest's `TEST(Suite, Name)`, `TEST_F` and `TEST_P`
/// (named `Suite.Name`, as `gtest_discover_tests` lists them), Catch2's
/// and doctest's `TEST_CASE("name")`. The outline's tests too
/// ([`crate::outline`]).
pub(crate) fn c_tests(lines: &[&str]) -> Vec<Runnable> {
    cpp_tests(lines)
}

fn cpp_tests(lines: &[&str]) -> Vec<Runnable> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        let name = if let Some(rest) = ["TEST(", "TEST_F(", "TEST_P("]
            .iter()
            .find_map(|m| t.strip_prefix(m))
        {
            let args: Vec<&str> = rest.split(')').next().unwrap_or("").split(',').collect();
            match args.as_slice() {
                [suite, name, ..] => format!("{}.{}", suite.trim(), name.trim()),
                _ => continue,
            }
        } else if let Some(rest) = ["TEST_CASE(", "SCENARIO("]
            .iter()
            .find_map(|m| t.strip_prefix(m))
        {
            match quoted(rest) {
                Some(name) => name,
                None => continue,
            }
        } else {
            continue;
        };
        let end = span_from(lines, i);
        out.push(test(name, i, end, Vec::new()));
    }
    out
}

// -- commands -------------------------------------------------------------------------------

/// The command that runs `runnable` — a test, a group, or a file's every
/// test (a group named for nothing) — of `file` in `lang`, under `root`,
/// as its project runs its tests. Programs are looked for on `search`.
pub fn test_command(
    root: &Path,
    file: &Path,
    lang: &str,
    source: &str,
    runnable: &Runnable,
    search: &OsStr,
) -> Result<JsCommand, String> {
    let tool = |name: &str| {
        crate::lsp::store::find_in_unresolved(search, name)
            .ok_or_else(|| format!("{other} wasn't found here.", other = name))
    };
    let whole_file = runnable.kind == RunnableKind::Tests && runnable.name.is_empty();
    let command = |program: PathBuf, args: Vec<String>, cwd: &Path| JsCommand {
        program,
        args,
        cwd: cwd.to_path_buf(),
        env: Vec::new(),
    };
    match lang {
        "go" => {
            let module =
                above(root, file, &["go.mod"]).ok_or_else(|| no_manifest("go.mod", file))?;
            let dir = file.parent().unwrap_or(&module);
            let package = format!("./{}", rel_slash(&module, dir));
            let mut args = vec!["test".to_string(), "-v".to_string()];
            match (whole_file, runnable.name.strip_prefix("Benchmark")) {
                // The file's own tests, not its package's every one.
                (true, _) => {
                    let names: Vec<String> = go_tests(&source.lines().collect::<Vec<_>>())
                        .into_iter()
                        .map(|t| t.name)
                        .filter(|n| !n.starts_with("Benchmark"))
                        .collect();
                    if !names.is_empty() {
                        args.push("-run".into());
                        args.push(format!("^({})$", names.join("|")));
                    }
                }
                (false, Some(_)) => {
                    args.extend(["-run".into(), "^$".into(), "-bench".into()]);
                    args.push(format!("^{}$", runnable.name));
                }
                (false, None) => {
                    args.push("-run".into());
                    args.push(format!("^{}$", runnable.name));
                }
            }
            args.push(package);
            Ok(command(tool("go")?, args, &module))
        }
        "java" | "kotlin" | "scala" => {
            let class = jvm_test_name(source, runnable);
            // The file's own tests: its test classes, not the project's
            // every one.
            let classes: Vec<String> = match &class {
                Some(_) => Vec::new(),
                None => file_test_classes(lang, source),
            };
            if let Some(dir) = above(root, file, &["build.sbt"]) {
                // One test by its framework's filter ([`jvm_test_filter`]),
                // a group by its class, a file by its classes.
                let quoted = |text: &str| format!("\"{}\"", text.replace('"', "\\\""));
                let filter = match (&class, jvm_test_filter(source, runnable, class.as_ref())) {
                    (Some((class, _)), Some(words)) => {
                        let words: Vec<String> = words
                            .iter()
                            .map(|w| match w.starts_with('-') {
                                true => w.clone(),
                                false => quoted(w),
                            })
                            .collect();
                        format!("testOnly {class} -- {}", words.join(" "))
                    }
                    (Some((class, _)), None) => format!("testOnly {class}"),
                    (None, _) if !classes.is_empty() => format!("testOnly {}", classes.join(" ")),
                    (None, _) => "test".into(),
                };
                return Ok(command(tool("sbt")?, vec![filter], &dir));
            }
            if let Some(dir) = above(root, file, &["build.mill", "build.sc"]) {
                let mut args = vec!["__.test.testOnly".to_string()];
                let words = jvm_test_filter(source, runnable, class.as_ref());
                match class {
                    Some((class, _)) => args.push(class),
                    None => args.extend(classes),
                }
                // One test by its framework's words, after `--`.
                if let Some(words) = words {
                    args.push("--".into());
                    args.extend(words);
                }
                return Ok(command(tool("mill")?, args, &dir));
            }
            if let Some(dir) = above(
                root,
                file,
                &[
                    "build.gradle.kts",
                    "build.gradle",
                    "settings.gradle.kts",
                    "settings.gradle",
                ],
            ) {
                let gradle = wrapper(root, &dir, "gradlew").map_or_else(|| tool("gradle"), Ok)?;
                // `cleanTest` first: a test run again, nothing changed,
                // was Gradle's "up to date", and ran nothing.
                let mut args = vec![
                    "-p".to_string(),
                    dir.to_string_lossy().into_owned(),
                    "cleanTest".into(),
                    "test".into(),
                ];
                if let Some((class, method)) = class {
                    args.push("--tests".into());
                    args.push(match method {
                        Some(method) => format!("{class}.{method}"),
                        None => class,
                    });
                }
                for class in classes {
                    args.extend(["--tests".into(), class]);
                }
                // Each test's outcome in the run (Gradle says only
                // "BUILD SUCCESSFUL", or points to a report, of its own).
                args.push("--init-script".into());
                args.push(gradle_test_script()?.to_string_lossy().into_owned());
                return Ok(command(gradle, args, &dir));
            }
            if let Some(dir) = above(root, file, &["pom.xml"]) {
                let maven = wrapper(root, &dir, "mvnw").map_or_else(|| tool("mvn"), Ok)?;
                let mut args = vec![
                    "-f".to_string(),
                    dir.join("pom.xml").to_string_lossy().into_owned(),
                    "test".into(),
                    "-Dsurefire.failIfNoSpecifiedTests=false".into(),
                ];
                if let Some((class, method)) = class {
                    args.push(match method {
                        Some(method) => format!("-Dtest={class}#{method}"),
                        None => format!("-Dtest={class}"),
                    });
                } else if !classes.is_empty() {
                    args.push(format!("-Dtest={}", classes.join(",")));
                }
                return Ok(command(maven, args, &dir));
            }
            if lang == "scala" {
                // A scala-cli project: its folder, or the file alone.
                let dir = file.parent().unwrap_or(root);
                let mut args = vec!["test".to_string(), dir.to_string_lossy().into_owned()];
                let words = jvm_test_filter(source, runnable, class.as_ref());
                if let Some((class, _)) = class {
                    args.extend(["--test-only".into(), class]);
                }
                for class in classes {
                    args.extend(["--test-only".into(), class]);
                }
                // The framework's own words after `--` (one test of the
                // suite).
                if let Some(words) = words {
                    args.push("--".into());
                    args.extend(words);
                }
                return Ok(command(tool("scala-cli")?, args, dir));
            }
            Err(no_manifest("build.gradle", file))
        }
        "csharp" => {
            let project = above_matching(root, file, |n| n.ends_with(".csproj"))
                .ok_or_else(|| no_manifest(".csproj", file))?;
            let dir = project.parent().unwrap_or(root).to_path_buf();
            let mut args = vec!["test".to_string(), project.to_string_lossy().into_owned()];
            if !whole_file {
                let namespace = csharp_namespace(source);
                let mut name: Vec<&str> = namespace.iter().map(String::as_str).collect();
                name.extend(runnable.scopes.iter().map(String::as_str));
                name.push(&runnable.name);
                args.push("--filter".into());
                args.push(format!("FullyQualifiedName~{}", name.join(".")));
            }
            Ok(command(tool("dotnet")?, args, &dir))
        }
        "dart" => {
            let package = above(root, file, &["pubspec.yaml"])
                .ok_or_else(|| no_manifest("pubspec.yaml", file))?;
            let flutter = crate::statefile::read_capped(&package.join("pubspec.yaml"), 1 << 20)
                .is_some_and(|p| p.contains("sdk: flutter"));
            let mut args = vec!["test".to_string(), rel_slash(&package, file)];
            if !whole_file {
                args.push("--plain-name".into());
                args.push(runnable.name.clone());
            }
            let runner = if flutter { "flutter" } else { "dart" };
            Ok(command(tool(runner)?, args, &package))
        }
        "ruby" => {
            let project = above(root, file, &["Gemfile"]);
            let dir = project.clone().unwrap_or_else(|| root.to_path_buf());
            let path = rel_slash(&dir, file);
            let bundled = |mut args: Vec<String>, program: &str| -> Result<JsCommand, String> {
                match &project {
                    Some(_) => {
                        args.insert(0, program.to_string());
                        args.insert(0, "exec".into());
                        Ok(command(tool("bundle")?, args, &dir))
                    }
                    None => Ok(command(tool(program)?, args, &dir)),
                }
            };
            if file.to_string_lossy().ends_with("_spec.rb") {
                let target = match whole_file {
                    true => path,
                    false => format!("{path}:{}", runnable.line),
                };
                return bundled(vec![target], "rspec");
            }
            if dir.join("bin/rails").is_file() {
                let target = match whole_file {
                    true => path,
                    false => format!("{path}:{}", runnable.line),
                };
                return Ok(command(
                    dir.join("bin/rails"),
                    vec!["test".into(), target],
                    &dir,
                ));
            }
            let mut args = vec!["-Itest".to_string(), "-Ilib".into(), path];
            if !whole_file {
                args.push("-n".into());
                args.push(runnable.name.clone());
            }
            bundled(args, "ruby")
        }
        "zig" => {
            let mut args = vec!["test".to_string(), file.to_string_lossy().into_owned()];
            if !whole_file {
                args.push("--test-filter".into());
                args.push(runnable.name.clone());
            }
            let dir = file.parent().unwrap_or(root);
            Ok(command(tool("zig")?, args, dir))
        }
        "erlang" => {
            let project = above(root, file, &["rebar.config"])
                .ok_or_else(|| no_manifest("rebar.config", file))?;
            let module = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let args = if module.ends_with("_SUITE") {
                let mut args = vec![
                    "ct".to_string(),
                    format!("--suite={}", rel_slash(&project, &file.with_extension(""))),
                ];
                if !whole_file {
                    args.push(format!("--case={}", runnable.name));
                }
                args
            } else if whole_file {
                vec!["eunit".to_string(), format!("--module={module}")]
            } else if runnable.name.ends_with("_test_") {
                vec![
                    "eunit".to_string(),
                    format!("--generator={module}:{}", runnable.name),
                ]
            } else {
                vec![
                    "eunit".to_string(),
                    format!("--test={module}:{}", runnable.name),
                ]
            };
            Ok(command(tool("rebar3")?, args, &project))
        }
        "elixir" => {
            let project =
                above(root, file, &["mix.exs"]).ok_or_else(|| no_manifest("mix.exs", file))?;
            let path = rel_slash(&project, file);
            let target = match whole_file {
                true => path,
                false => format!("{path}:{}", runnable.line),
            };
            Ok(command(tool("mix")?, vec!["test".into(), target], &project))
        }
        "haskell" => {
            let pattern = (!whole_file).then(|| runnable.name.clone());
            let tasty = source.contains("Test.Tasty");
            let flag = if tasty { "--pattern" } else { "--match" };
            if let Some(dir) = above(root, file, &["stack.yaml"]) {
                let mut args = vec!["test".to_string()];
                if let Some(pattern) = pattern {
                    args.push("--ta".into());
                    args.push(format!("{flag} \"{pattern}\""));
                }
                return Ok(command(tool("stack")?, args, &dir));
            }
            let dir = above_matching(root, file, |n| {
                n == "cabal.project" || n.ends_with(".cabal")
            })
            .and_then(|f| f.parent().map(Path::to_path_buf))
            .ok_or_else(|| no_manifest(".cabal", file))?;
            let mut args = vec!["test".to_string(), "--test-show-details=direct".into()];
            if let Some(pattern) = pattern {
                args.push(format!("--test-option={flag}"));
                args.push(format!("--test-option={pattern}"));
            }
            Ok(command(tool("cabal")?, args, &dir))
        }
        "c" | "cpp" => {
            let Some(build) = ctest_dir(root) else {
                return Err("This project's tests run by CTest from a CMake build: build it, then run the test again.".to_string());
            };
            let mut args = vec![
                "--test-dir".to_string(),
                build.to_string_lossy().into_owned(),
                "--output-on-failure".into(),
            ];
            if !whole_file {
                args.push("-R".into());
                args.push(regex_literal(&runnable.name));
            }
            Ok(command(tool("ctest")?, args, root))
        }
        _ => Err(format!("{rel} has no tests to run", rel = lang)),
    }
}

/// A test debugged: the program its debugger is given, and how the
/// debugger is started for a test rather than a program.
#[derive(Debug, Clone, PartialEq)]
pub struct DebugTest {
    /// The debugger, by [`crate::debugadapter::resolve`]'s name for its
    /// language: `go`, `ruby`, `dart`, `java` or `native`.
    pub adapter: &'static str,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Set on the debugger's `launch` over what it asks for a program:
    /// Delve's test mode, the build the JVM debugger runs.
    pub launch: serde_json::Map<String, serde_json::Value>,
    /// How the debugger itself is started for it.
    pub start: DebugStart,
}

/// How a debugger is started for a test ([`DebugTest`]).
#[derive(Debug, Clone, PartialEq)]
pub enum DebugStart {
    /// As for a program.
    Program,
    /// rdbg running a command (`bundle exec rspec …`), not a script.
    RubyCommand,
    /// Dart's test adapter (`dart debug_adapter --test`) — Flutter's, by
    /// the `flutter` given, for a Flutter package's tests.
    DartTests { flutter: Option<PathBuf> },
    /// `dotnet test` with its test host waiting for netcoredbg to attach:
    /// clew-server's `dotnet-test-debug` in front of netcoredbg.
    DotnetTests,
}

/// The JVM's debugging agent as a test's JVM is started under it: held at
/// its start, listening on a free loopback port it announces
/// (`Listening for transport dt_socket at address: …`).
pub const JDWP_AGENT: &str =
    "-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=127.0.0.1:0";

/// The debugger a test of `lang` in `file` is debugged under, where Clew
/// has one that runs it — [`crate::debugadapter::resolve`]'s name for it —
/// and none where its tests run without one: C#'s, Erlang's, Elixir's,
/// Haskell's, and a JVM build's that runs them out of reach (Mill, a
/// scala-cli folder, an sbt build that forks them). Cheap: it reads only
/// which build files there are.
pub fn test_debugger(root: &Path, file: &Path, lang: &str) -> Option<&'static str> {
    match lang {
        "go" => Some("go"),
        "ruby" => Some("ruby"),
        "dart" => Some("dart"),
        "csharp" => Some("csharp"),
        "zig" => Some("native"),
        "c" | "cpp" => ctest_dir(root).map(|_| "native"),
        "java" | "kotlin" | "scala" => jvm_debug_build(root, file)
            .or_else(|| scala_cli_tests(root, file, lang))
            .map(|_| "java"),
        // EUnit's, not a Common Test suite's: CT runs its cases its own way.
        "erlang" => {
            let suite = file
                .file_stem()
                .is_some_and(|s| s.to_string_lossy().ends_with("_SUITE"));
            (!suite).then_some("erlang")
        }
        "elixir" => above(root, file, &["mix.exs"]).map(|_| "elixir"),
        // A Cabal test suite's: its `main`, under GHCi's debugger.
        "haskell" => haskell_suite(root, file).map(|_| "haskell"),
        _ => None,
    }
}

/// The Cabal test suite `file` is in: its project's folder, its name, and
/// its `main-is` file — the suite whose `hs-source-dirs` hold the file,
/// or the project's only one.
fn haskell_suite(root: &Path, file: &Path) -> Option<(PathBuf, String, PathBuf)> {
    let cabal = above_matching(root, file, |n| n.ends_with(".cabal"))?;
    let dir = cabal.parent()?.to_path_buf();
    let text = crate::statefile::read_capped(&cabal, 1 << 20)?;
    let mut suites: Vec<(String, Option<String>, Vec<String>)> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        let field = |name: &str| {
            t.to_ascii_lowercase()
                .starts_with(&format!("{name}:"))
                .then(|| t[name.len() + 1..].trim().to_string())
        };
        if !line.starts_with([' ', '\t']) {
            let lower = t.to_ascii_lowercase();
            if let Some(name) = lower.strip_prefix("test-suite ") {
                let name = t[t.len() - name.len()..].trim().to_string();
                suites.push((name, None, Vec::new()));
            } else if !t.is_empty() && !t.starts_with("--") {
                // Another stanza, or a top-level field: the suite ended.
                suites.push((String::new(), None, Vec::new()));
            }
            continue;
        }
        let Some(suite) = suites.last_mut().filter(|s| !s.0.is_empty()) else {
            continue;
        };
        if let Some(main) = field("main-is") {
            suite.1 = Some(main);
        } else if let Some(dirs) = field("hs-source-dirs") {
            suite.2.extend(
                dirs.split([',', ' '])
                    .filter(|d| !d.is_empty())
                    .map(str::to_string),
            );
        }
    }
    let suites: Vec<_> = suites
        .into_iter()
        .filter(|(name, main, _)| !name.is_empty() && main.is_some())
        .collect();
    let holds = |dirs: &[String]| dirs.iter().any(|d| file.starts_with(dir.join(d)));
    let (name, main, dirs) = match suites.iter().find(|(_, _, dirs)| holds(dirs)) {
        Some(suite) => suite.clone(),
        None if suites.len() == 1 => suites[0].clone(),
        None => return None,
    };
    let main = main?;
    let path = dirs
        .iter()
        .map(|d| dir.join(d).join(&main))
        .find(|p| p.is_file())
        .unwrap_or_else(|| dir.join(&main));
    Some((dir, name, path))
}

/// The build a JVM test is debugged through.
#[derive(Debug, Clone, Copy, PartialEq)]
enum JvmBuild {
    Gradle,
    Maven,
    Sbt,
    /// A Scala file with no build of its own, its tests by scala-cli.
    ScalaCli,
    /// Mill's: its tests run in its own JVM (`testLocal`), given the agent.
    Mill,
}

/// The build that runs `file`'s tests where the JVM debugger reaches them,
/// found as [`test_command`] finds it: Gradle's, Maven's and sbt's, each
/// test JVM started under the agent (sbt's forked for it).
fn jvm_debug_build(root: &Path, file: &Path) -> Option<JvmBuild> {
    if above(root, file, &["build.sbt"]).is_some() {
        return Some(JvmBuild::Sbt);
    }
    if above(root, file, &["build.mill", "build.sc"]).is_some() {
        return Some(JvmBuild::Mill);
    }
    if above(
        root,
        file,
        &[
            "build.gradle.kts",
            "build.gradle",
            "settings.gradle.kts",
            "settings.gradle",
        ],
    )
    .is_some()
    {
        return Some(JvmBuild::Gradle);
    }
    above(root, file, &["pom.xml"]).map(|_| JvmBuild::Maven)
}

/// The Mill module a source file is in, by its place under the build's
/// folder: the folders before its `src` (`foo/src/Main.scala` is `foo`,
/// `foo/test/src/…` `foo.test`, Maven's `foo/src/main/scala/…` `foo`).
fn mill_module(build: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(build).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .take_while(|p| p != "src")
        .collect();
    (!parts.is_empty() && parts.len() < rel.components().count()).then(|| parts.join("."))
}

/// The JVM options file Mill's own JVM starts with (`MILL_JVM_OPTS_PATH`)
/// for a run under the debugger: the build's own options — its
/// `.mill-jvm-opts`, or the file the variable already names — then the
/// `agent`'s, written to Clew's data directory. Mill's launcher JVM, which
/// `JAVA_OPTS` reaches, is not the one that runs the code.
fn mill_jvm_opts(dir: &Path, agent: &str, port: u16) -> Result<PathBuf, String> {
    let own = std::env::var_os("MILL_JVM_OPTS_PATH")
        .filter(|p| !p.is_empty())
        .map(|p| dir.join(p))
        .unwrap_or_else(|| dir.join(".mill-jvm-opts"));
    let mut opts = std::fs::read_to_string(&own).unwrap_or_default();
    if !opts.is_empty() && !opts.ends_with('\n') {
        opts.push('\n');
    }
    opts.push_str(agent);
    opts.push('\n');
    let folder = crate::lsp::store::data_root()
        .unwrap_or_else(std::env::temp_dir)
        .join("debug-adapters")
        .join("mill");
    std::fs::create_dir_all(&folder)
        .map_err(|e| format!("cannot write {}: {e}", folder.display()))?;
    // Earlier runs' files, read long since, go.
    if let Ok(entries) = std::fs::read_dir(&folder) {
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > std::time::Duration::from_secs(3600));
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let path = folder.join(format!("jvm-opts-{port}"));
    std::fs::write(&path, opts).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// The words Mill's `testLocal` hands the test framework to run `runnable`
/// alone: MUnit's full name (`shop.CartSuite.adds`, a suite's
/// `shop.CartSuite.*`), ScalaTest's suite (`-s`) and test (`-z`); a file
/// its suites'. None for a framework that has none: the module's tests.
fn mill_local_filter(source: &str, runnable: &Runnable, lang: &str) -> Vec<String> {
    let class = jvm_test_name(source, runnable);
    let classes = match &class {
        Some((class, _)) => vec![class.clone()],
        None => file_test_classes(lang, source),
    };
    if source.contains("org.scalatest") {
        let mut words: Vec<String> = classes
            .iter()
            .flat_map(|c| ["-s".to_string(), c.clone()])
            .collect();
        if runnable.kind == RunnableKind::Test {
            words.extend(["-z".to_string(), runnable.name.clone()]);
        }
        return words;
    }
    if source.contains("munit.") {
        return match (&class, runnable.kind) {
            (Some((class, Some(name))), RunnableKind::Test) => vec![format!("{class}.{name}")],
            _ => classes.iter().map(|c| format!("{c}.*")).collect(),
        };
    }
    Vec::new()
}

/// A Scala file's tests run by scala-cli: no build of its own around it.
fn scala_cli_tests(root: &Path, file: &Path, lang: &str) -> Option<JvmBuild> {
    let built = above(
        root,
        file,
        &[
            "build.sbt",
            "build.mill",
            "build.sc",
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
            "pom.xml",
        ],
    );
    (lang == "scala" && built.is_none()).then_some(JvmBuild::ScalaCli)
}

/// sbt's commands that run `task`'s JVM (`Test`, `run`) forked, with the
/// debugger's agent on it alone — every project's (`ThisBuild`): sbt's
/// own JVM, and the one its launcher starts first to ask Java for its
/// runtime, are not the program's (the debugger attached to that one, it
/// ended, and the program's waited for a debugger that never came).
fn sbt_forked_under(task: &str, agent: &str) -> [String; 2] {
    // Commands to sbt, not words to the reader: built, not formatted.
    [
        ["set ThisBuild / ", task, " / fork := true"].concat(),
        [
            "set ThisBuild / ",
            task,
            " / javaOptions += \"",
            agent,
            "\"",
        ]
        .concat(),
    ]
}

/// The debugger a program of `lang` in `file` is debugged under, run as
/// Run runs it ([`main_command`]): Kotlin's and Scala's JVM — a Gradle
/// build's `run`, a script's `kotlinc`, sbt's `run` in its own JVM,
/// scala-cli's — and Elixir's under OTP's interpreter, in its Mix project
/// or alone. None where it runs out of the debugger's reach (an sbt build
/// that forks it) or has no build to run it.
pub fn main_debugger(root: &Path, file: &Path, lang: &str) -> Option<&'static str> {
    let name = file.to_string_lossy();
    match lang {
        "kotlin" if name.ends_with(".kts") => Some("java"),
        "kotlin" => above(
            root,
            file,
            &[
                "build.gradle.kts",
                "build.gradle",
                "settings.gradle.kts",
                "settings.gradle",
            ],
        )
        .map(|_| "java"),
        "scala" => Some("java"),
        "elixir" => Some("elixir"),
        // A .NET program, built and run under netcoredbg; a Zig file with
        // a `main`, built for the session under lldb — alone, or by its
        // project's build.zig where it does not build alone.
        "csharp" => dotnet_project(root, file)
            .filter(|(_, program)| *program)
            .map(|_| "csharp"),
        "zig" => has_zig_main(file).then_some("native"),
        // A CMake target's source, or a file with a `main` built alone; a
        // make, Meson or Bazel project's program is a launch file's to name.
        "c" | "cpp" => {
            let cmake = cmake_target(root, file).is_some();
            (cmake
                || (!other_c_build(root, file)
                    && above(root, file, &["CMakeLists.txt"]).is_none()
                    && has_c_main(file, lang)))
            .then_some("native")
        }
        _ => None,
    }
}

/// How a program of `lang` in `file` is debugged: run as Run runs it
/// ([`main_command`]), under its language's debugger — the JVM's agent on
/// Gradle's `run` (Clew's init script), on `kotlinc`'s JVM (`JAVA_OPTS`),
/// on sbt's own JVM (`-J`), on scala-cli's (`--java-opt`); Elixir's under
/// OTP's interpreter, its Mix project's application started first (and
/// kept running, for an application with a supervision tree, as `mix run
/// --no-halt` keeps it), a script required.
pub fn debug_main(
    root: &Path,
    file: &Path,
    lang: &str,
    search: &OsStr,
    said: &mut dyn FnMut(&str),
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<DebugTest, String> {
    if matches!(lang, "c" | "cpp") {
        let (program, cwd) = build_c_program(root, file, lang, search, said, cancel)?;
        return Ok(DebugTest {
            adapter: "native",
            program,
            args: Vec::new(),
            cwd,
            launch: serde_json::Map::new(),
            start: DebugStart::Program,
        });
    }
    let run = main_command(root, file, lang, search)?;
    let name = file.to_string_lossy().into_owned();
    let jvm = |program: PathBuf, args: Vec<String>, cwd: PathBuf| {
        let mut command = vec![serde_json::Value::from(
            program.to_string_lossy().into_owned(),
        )];
        command.extend(args.iter().cloned().map(serde_json::Value::from));
        let mut launch = serde_json::Map::new();
        launch.insert("command".into(), command.into());
        DebugTest {
            adapter: "java",
            program,
            args,
            cwd,
            launch,
            start: DebugStart::Program,
        }
    };
    match lang {
        "kotlin" if name.ends_with(".kts") => {
            // kotlinc runs the script in its own JVM, started with
            // `JAVA_OPTS`.
            let opts = match std::env::var("JAVA_OPTS") {
                Ok(opts) if !opts.trim().is_empty() => format!("{opts} {JDWP_AGENT}"),
                _ => JDWP_AGENT.to_string(),
            };
            let mut debug = jvm(run.program, run.args, run.cwd);
            debug
                .launch
                .insert("env".into(), serde_json::json!({ "JAVA_OPTS": opts }));
            Ok(debug)
        }
        "kotlin" => {
            let mut args = run.args;
            args.push("--init-script".into());
            args.push(gradle_debug_script()?.to_string_lossy().into_owned());
            Ok(jvm(run.program, args, run.cwd))
        }
        "scala" => {
            // sbt's JVM, or the one scala-cli starts, on a port of Clew's:
            // what they print of the agent is not counted on.
            let mut args = run.args;
            let port = free_port()?;
            let agent = JDWP_AGENT.replace("127.0.0.1:0", &format!("127.0.0.1:{port}"));
            let tool = run
                .program
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut env = None;
            match tool.as_str() {
                "sbt" => {
                    for (i, set) in sbt_forked_under("run", &agent).into_iter().enumerate() {
                        args.insert(i, set);
                    }
                }
                // Mill's program in its own JVM (`runMainLocal`, with no
                // server), started with the agent in its options file: it
                // forks `runMain`'s where the agent can't be put.
                "mill" => {
                    args = args
                        .into_iter()
                        .map(|a| match a.strip_suffix(".runMain") {
                            Some(module) => format!("{module}.runMainLocal"),
                            None => match a.strip_suffix(".run") {
                                Some(module) => format!("{module}.runLocal"),
                                None => a,
                            },
                        })
                        .collect();
                    args.insert(0, "--no-server".into());
                    env = Some(mill_jvm_opts(&run.cwd, &agent, port)?);
                }
                _ => args.extend(["--java-opt".to_string(), agent]),
            }
            let mut debug = jvm(run.program, args, run.cwd);
            debug.launch.insert("port".into(), port.into());
            if let Some(opts) = env {
                debug.launch.insert(
                    "env".into(),
                    serde_json::json!({ "MILL_JVM_OPTS_PATH": opts.to_string_lossy() }),
                );
            }
            Ok(debug)
        }
        "csharp" => {
            // Built for debugging, then its assembly — the build's `Name
            // -> …/Name.dll` — run under netcoredbg.
            let (project, _) =
                dotnet_project(root, file).ok_or_else(|| no_program(".csproj", file))?;
            let dir = project.parent().unwrap_or(root).to_path_buf();
            let mut build = crate::spawn::command(&run.program);
            build
                .args(["build", "--nologo", "-c", "Debug"])
                .arg(&project)
                .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
                .env("DOTNET_NOLOGO", "1")
                .current_dir(&dir);
            let (built, lines) = build_step(build, said, cancel)?;
            if !built {
                return Err(format!(
                    "dotnet could not build {project} (its output is above).",
                    project = project
                        .file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_default(),
                ));
            }
            let assembly = lines
                .iter()
                .filter_map(|l| l.split_once(" -> ").map(|(_, p)| p.trim()))
                .find(|p| p.ends_with(".dll"))
                .map(PathBuf::from)
                .ok_or_else(|| not_a_program(&project))?;
            Ok(DebugTest {
                adapter: "csharp",
                program: assembly,
                args: Vec::new(),
                cwd: dir,
                launch: serde_json::Map::new(),
                start: DebugStart::Program,
            })
        }
        // Run where `zig build run` runs it: its build.zig's folder.
        "zig" => {
            let dir = above(root, file, &["build.zig"])
                .unwrap_or_else(|| file.parent().unwrap_or(root).to_path_buf());
            Ok(DebugTest {
                adapter: "native",
                program: file.to_path_buf(),
                args: Vec::new(),
                cwd: dir,
                launch: serde_json::Map::new(),
                start: DebugStart::Program,
            })
        }
        "elixir" => {
            let project = above(root, file, &["mix.exs"]);
            let path = elixir_string(&name);
            let start = "Mix.Task.rerun(\"app.start\")\n";
            let code = match (&project, name.ends_with(".exs")) {
                (Some(_), true) => format!("{start}Code.require_file({path})\n0"),
                (Some(dir), false) => {
                    let serves = crate::statefile::read_capped(&dir.join("mix.exs"), 1 << 20)
                        .is_some_and(|mix| mix.contains("mod:"));
                    let held = if serves {
                        "Process.sleep(:infinity)\n"
                    } else {
                        ""
                    };
                    format!("{start}{held}0")
                }
                (None, _) => format!("Code.require_file({path})\n0"),
            };
            let mut launch = serde_json::Map::new();
            launch.insert("language".into(), "elixir".into());
            if let Some(dir) = &project {
                launch.insert(
                    "projectDir".into(),
                    dir.to_string_lossy().into_owned().into(),
                );
            }
            launch.insert("mixEnv".into(), "dev".into());
            launch.insert("run".into(), code.into());
            Ok(DebugTest {
                adapter: "elixir",
                program: file.to_path_buf(),
                args: Vec::new(),
                cwd: run.cwd,
                launch,
                start: DebugStart::Program,
            })
        }
        _ => Err(format!("{rel} has no program to run", rel = lang)),
    }
}

/// How `runnable`, a test of `file`, is debugged: run as [`test_command`]
/// runs it, under its language's debugger — Go's by Delve's test mode,
/// Ruby's command under rdbg, Dart's and Flutter's by their test adapters,
/// a JVM build's with each test JVM under the debugging agent (Gradle by
/// an init script of Clew's, Maven by Surefire's `maven.surefire.debug`,
/// sbt's own JVM by `-J`), a CTest test's own program and arguments (as
/// CTest lists them) under lldb, a Zig test built alone for it. It may run
/// CTest or build a Zig test: off the UI thread.
pub fn debug_test(
    root: &Path,
    file: &Path,
    lang: &str,
    source: &str,
    runnable: &Runnable,
    search: &OsStr,
    choices: &clew_protocol::LanguageChoices,
) -> Result<DebugTest, String> {
    let whole_file = runnable.kind == RunnableKind::Tests && runnable.name.is_empty();
    let test = |adapter, program, args, cwd| DebugTest {
        adapter,
        program,
        args,
        cwd,
        launch: serde_json::Map::new(),
        start: DebugStart::Program,
    };
    match lang {
        "c" | "cpp" => return ctest_debug(root, file, lang, source, runnable, search),
        "erlang" => {
            // EUnit's own call for the test, its module or its generator,
            // under the interpreter, the file compiled with `TEST` as
            // rebar3's `eunit` compiles it.
            let module = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Code, not words: put together, not formatted.
            let m = erlang_atom(&module);
            let tests = match (whole_file, runnable.name.ends_with("_test_")) {
                // Each of its tests, held: a timeout around the module
                // is not its tests' (they keep EUnit's five seconds).
                (true, _) => [
                    "[",
                    &eunit_held(
                        &[
                            "case lists:suffix(\"_test_\", atom_to_list(F)) of true -> {generator, ",
                            &m,
                            ", F}; false -> {test, ",
                            &m,
                            ", F} end",
                        ]
                        .concat(),
                    ),
                    " || {F, 0} <- ",
                    &m,
                    ":module_info(exports), lists:suffix(\"_test\", atom_to_list(F)) orelse lists:suffix(\"_test_\", atom_to_list(F))]",
                ]
                .concat(),
                (false, true) => eunit_held(
                    &["{generator, ", &m, ", ", &erlang_atom(&runnable.name), "}"].concat(),
                ),
                (false, false) => {
                    eunit_held(&["{test, ", &m, ", ", &erlang_atom(&runnable.name), "}"].concat())
                }
            };
            let call = ["eunit:test(", &tests, ", [verbose])"].concat();
            // In its rebar3 project, where its dependencies are built; rebar3
            // itself is not needed.
            let cwd = above(root, file, &["rebar.config"])
                .unwrap_or_else(|| file.parent().unwrap_or(root).to_path_buf());
            let mut debug = test("erlang", file.to_path_buf(), Vec::new(), cwd);
            debug.launch.insert("run".into(), call.into());
            debug
                .launch
                .insert("defines".into(), serde_json::json!(["TEST"]));
            return Ok(debug);
        }
        "zig" => {
            let zig = crate::debugadapter::zig_for(choices)?;
            let filter = (!whole_file).then_some(runnable.name.as_str());
            let dir = file.parent().unwrap_or(root);
            let binary = crate::debugadapter::build_zig_test(&zig, file, filter, dir)?;
            return Ok(test("native", binary, Vec::new(), dir.to_path_buf()));
        }
        _ => {}
    }
    let run = test_command(root, file, lang, source, runnable, search)?;
    match lang {
        "go" => {
            // Delve builds the package's tests and runs them, in its
            // folder, as `go test` does: asked for the test as `go test`
            // asks, in the test binary's own words.
            let dir = file.parent().unwrap_or(root).to_path_buf();
            let args = run
                .args
                .iter()
                .skip(1)
                .filter(|a| !a.starts_with("./"))
                .map(|a| match a.as_str() {
                    "-v" | "-run" | "-bench" => format!("-test.{}", &a[1..]),
                    _ => a.clone(),
                })
                .collect();
            let mut debug = test("go", dir.clone(), args, dir);
            debug.launch.insert("mode".into(), "test".into());
            Ok(debug)
        }
        "ruby" => {
            let mut debug = test("ruby", run.program, run.args, run.cwd);
            debug.start = DebugStart::RubyCommand;
            Ok(debug)
        }
        "haskell" => {
            // The suite's own `main`, under GHCi's debugger in the suite's
            // REPL (`cabal repl test:spec`, `stack ghci`), asked for the
            // test as Run Test asks (`--match`, Tasty's `--pattern`).
            let Some((dir, suite, main)) = haskell_suite(root, file) else {
                return Err(format!("{rel} has no tests to run", rel = lang));
            };
            let ghci = match above(root, file, &["stack.yaml"]) {
                Some(_) => {
                    let package = std::fs::read_dir(&dir)
                        .ok()
                        .into_iter()
                        .flatten()
                        .flatten()
                        .find_map(|e| {
                            let name = e.file_name().to_string_lossy().into_owned();
                            name.strip_suffix(".cabal").map(str::to_string)
                        })
                        .unwrap_or_default();
                    ["stack ghci ", &package, ":test:", &suite].concat()
                }
                // Commands, not words: put together, not formatted.
                None => ["cabal repl test:", &suite].concat(),
            };
            let mut args = Vec::new();
            if !whole_file {
                let tasty = source.contains("Test.Tasty");
                args.push(if tasty { "--pattern" } else { "--match" }.to_string());
                args.push(runnable.name.clone());
            }
            let mut debug = test("haskell", main, args, dir);
            debug.launch.insert("ghci".into(), ghci.into());
            Ok(debug)
        }
        "csharp" => {
            // `dotnet test` as Run Test runs it, its test host waiting for
            // netcoredbg to attach (VSTest's host debugging).
            let mut command = vec![serde_json::Value::from(
                run.program.to_string_lossy().into_owned(),
            )];
            command.extend(run.args.iter().cloned().map(serde_json::Value::from));
            let mut debug = test("csharp", run.program, run.args, run.cwd);
            debug.launch.insert("command".into(), command.into());
            debug.start = DebugStart::DotnetTests;
            Ok(debug)
        }
        "dart" => {
            let flutter = run
                .program
                .file_stem()
                .is_some_and(|n| n.to_string_lossy() == "flutter");
            let mut args = Vec::new();
            if !whole_file {
                args.push("--plain-name".into());
                args.push(runnable.name.clone());
            }
            let mut debug = test("dart", file.to_path_buf(), args, run.cwd);
            debug.start = DebugStart::DartTests {
                flutter: flutter.then_some(run.program),
            };
            Ok(debug)
        }
        "elixir" => {
            // As `mix test file:line` runs it, in the test environment:
            // the application started, ExUnit's helper required, the
            // test file compiled (its modules interpreted, for its
            // breakpoints), the test picked by its line.
            let project = run.cwd.clone();
            let rel = rel_slash(&project, file);
            let load = format!(
                "Mix.Task.rerun(\"app.start\")\n\
                 ExUnit.start(autorun: false)\n\
                 for path <- Mix.Project.config()[:test_paths] || [\"test\"],\n    \
                 helper = Path.join(path, \"test_helper.exs\"),\n    \
                 File.exists?(helper),\n    \
                 do: Code.require_file(helper)\n\
                 for file <- [{file}], {{mod, bin}} <- Code.compile_file(file), do: {{mod, file, bin}}",
                file = elixir_string(&file.to_string_lossy())
            );
            let filter = match whole_file {
                true => String::new(),
                false => format!(
                    "{{_, filters}} = ExUnit.Filters.parse_path({target})\n\
                     opts = if Keyword.has_key?(filters, :include), do: filters, else: [exclude: [:test], include: filters]\n\
                     if filters != [], do: ExUnit.configure(opts)\n",
                    target = elixir_string(&format!("{rel}:{}", runnable.line))
                ),
            };
            // ExUnit fails a test after a minute, and a test paused at a
            // breakpoint failed under the person reading its frame: held
            // as long as it is read, unless the test sets its own.
            let run = format!(
                "ExUnit.configure(timeout: :infinity)\n{filter}%{{failures: failures}} = ExUnit.run()\nif failures > 0, do: 1, else: 0"
            );
            let mut debug = test("elixir", file.to_path_buf(), Vec::new(), project.clone());
            debug.launch.insert("language".into(), "elixir".into());
            debug.launch.insert(
                "projectDir".into(),
                project.to_string_lossy().into_owned().into(),
            );
            debug.launch.insert("mixEnv".into(), "test".into());
            debug.launch.insert("load".into(), load.into());
            debug.launch.insert("run".into(), run.into());
            Ok(debug)
        }
        "java" | "kotlin" | "scala" => {
            let build = jvm_debug_build(root, file).or_else(|| scala_cli_tests(root, file, lang));
            let Some(build) = build else {
                return Err("This build runs its tests where the debugger can't reach them, so they run without it.".to_string());
            };
            let mut args = run.args;
            let mut agent_port: Option<u16> = None;
            let mut mill_env: Option<PathBuf> = None;
            match build {
                JvmBuild::Gradle => {
                    args.push("--init-script".into());
                    args.push(gradle_debug_script()?.to_string_lossy().into_owned());
                }
                // Surefire keeps its test JVM's word of where the agent
                // listens to itself: the agent is given a port to listen
                // on, which the debugger attaches to once it does.
                JvmBuild::Maven => {
                    let port = free_port()?;
                    let agent = JDWP_AGENT.replace("127.0.0.1:0", &format!("127.0.0.1:{port}"));
                    args.push(format!("-Dmaven.surefire.debug={agent}"));
                    agent_port = Some(port);
                }
                // sbt's tests in a JVM of their own, on a port of Clew's
                // too: what sbt prints is not counted on.
                JvmBuild::Sbt => {
                    let port = free_port()?;
                    let agent = JDWP_AGENT.replace("127.0.0.1:0", &format!("127.0.0.1:{port}"));
                    for (i, set) in sbt_forked_under("Test", &agent).into_iter().enumerate() {
                        args.insert(i, set);
                    }
                    agent_port = Some(port);
                }
                // scala-cli's test JVM given the agent (`--java-opt`), on a
                // port of Clew's, before the framework's own words.
                JvmBuild::ScalaCli => {
                    let port = free_port()?;
                    let agent = JDWP_AGENT.replace("127.0.0.1:0", &format!("127.0.0.1:{port}"));
                    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
                    args.splice(at..at, ["--java-opt".to_string(), agent]);
                    agent_port = Some(port);
                }
                // Mill forks its tests where the agent can't be put from
                // its command line: they run in its own JVM instead
                // (`testLocal`, `--no-server`), started with the agent in
                // its options file, on a port of Clew's.
                JvmBuild::Mill => {
                    let dir = above(root, file, &["build.mill", "build.sc"])
                        .unwrap_or_else(|| root.to_path_buf());
                    let module = mill_module(&dir, file).ok_or_else(|| no_main(file))?;
                    args = vec!["--no-server".into(), format!("{module}.testLocal")];
                    args.extend(mill_local_filter(source, runnable, lang));
                    let port = free_port()?;
                    let agent = JDWP_AGENT.replace("127.0.0.1:0", &format!("127.0.0.1:{port}"));
                    mill_env = Some(mill_jvm_opts(&run.cwd, &agent, port)?);
                    agent_port = Some(port);
                }
            }
            let mut command = vec![serde_json::Value::from(
                run.program.to_string_lossy().into_owned(),
            )];
            command.extend(args.iter().cloned().map(serde_json::Value::from));
            let mut debug = test("java", run.program, args, run.cwd);
            debug.launch.insert("command".into(), command.into());
            if let Some(port) = agent_port {
                debug.launch.insert("port".into(), port.into());
            }
            if let Some(opts) = mill_env {
                debug.launch.insert(
                    "env".into(),
                    serde_json::json!({ "MILL_JVM_OPTS_PATH": opts.to_string_lossy() }),
                );
            }
            Ok(debug)
        }
        _ => Err(format!("{rel} has no tests to run", rel = lang)),
    }
}

/// EUnit's `test` held as long as it is read: EUnit cancels a test after
/// five seconds, and a test paused at a breakpoint was cancelled under the
/// person reading its frame ("*timed out*"). A day, as EUnit takes no
/// `infinity`; a test's own `{timeout, …}` inside still holds.
fn eunit_held(test: &str) -> String {
    ["{timeout, 86400, ", test, "}"].concat()
}

/// `name` as an Erlang atom, quoted.
fn erlang_atom(name: &str) -> String {
    format!("'{}'", name.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// `text` as an Elixir string: its quotes, backslashes and interpolation
/// escaped.
fn elixir_string(text: &str) -> String {
    let mut quoted = String::from("\"");
    for c in text.chars() {
        match c {
            '"' | '\\' | '#' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '\n' => quoted.push_str("\\n"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// A loopback port free now, for an agent to listen on.
fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("no free port for the debugger: {e}"))
}

/// Gradle's init script that has its test tasks say each test's outcome
/// as it ends (`CartTest > adds() PASSED`), a failure's assertion and stack
/// in full, and what a test prints — written once to Clew's data
/// directory.
fn gradle_test_script() -> Result<PathBuf, String> {
    const SCRIPT: &str = "// Written by Clew to run tests: each test's outcome said as it ends.\ngradle.allprojects { project ->\n    project.tasks.withType(Test).configureEach { task ->\n        task.testLogging {\n            events 'passed', 'skipped', 'failed'\n            exceptionFormat 'full'\n            showStandardStreams true\n        }\n    }\n}\n";
    write_gradle_script("clew-tests.gradle", SCRIPT)
}

/// A Gradle init script of Clew's, written to its data directory where it
/// is not there already as it should be.
fn write_gradle_script(name: &str, script: &str) -> Result<PathBuf, String> {
    let dir = crate::lsp::store::data_root()
        .unwrap_or_else(std::env::temp_dir)
        .join("debug-adapters")
        .join("gradle");
    let path = dir.join(name);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(script) {
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(&path, script))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    Ok(path)
}

/// Gradle's init script that starts each test JVM, and an application's
/// `run`, under the debugging agent — one JVM at a time — written once to
/// Clew's data directory.
fn gradle_debug_script() -> Result<PathBuf, String> {
    const SCRIPT: &str = "// Written by Clew to debug a test or a program: its JVM starts under\n// the debugging agent, held until the debugger attaches.\ngradle.allprojects { project ->\n    project.tasks.withType(Test).configureEach { task ->\n        task.jvmArgs '-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=127.0.0.1:0'\n        task.maxParallelForks = 1\n    }\n    project.tasks.withType(JavaExec).configureEach { task ->\n        if (task.name == 'run') {\n            task.jvmArgs '-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=127.0.0.1:0'\n        }\n    }\n}\n";
    write_gradle_script("clew-debug.gradle", SCRIPT)
}

/// A CTest test debugged: the program CTest runs for it and its arguments
/// (GoogleTest's `--gtest_filter`, Catch2's test name), in its working
/// directory, as `ctest --show-only=json-v1` lists them. A file's tests in
/// one program run together, by one filter naming them all.
fn ctest_debug(
    root: &Path,
    file: &Path,
    lang: &str,
    source: &str,
    runnable: &Runnable,
    search: &OsStr,
) -> Result<DebugTest, String> {
    let Some(build) = ctest_dir(root) else {
        return Err("This project's tests run by CTest from a CMake build: build it, then run the test again.".to_string());
    };
    let ctest = crate::lsp::store::find_in_unresolved(search, "ctest")
        .ok_or_else(|| format!("{other} wasn't found here.", other = "ctest"))?;
    let mut cmd = crate::spawn::command(&ctest);
    cmd.arg("--test-dir")
        .arg(&build)
        .arg("--show-only=json-v1")
        .current_dir(root);
    let out = crate::lsp::store::run_bounded(cmd, std::time::Duration::from_secs(60))?;
    let listed: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|_| "CTest's list of tests could not be read.".to_string())?;
    let whole_file = runnable.kind == RunnableKind::Tests && runnable.name.is_empty();
    let names: Vec<String> = match whole_file {
        true => tests(lang, &file.to_string_lossy(), source)
            .into_iter()
            .filter(|t| t.kind == RunnableKind::Test)
            .map(|t| t.name)
            .collect(),
        false => vec![runnable.name.clone()],
    };
    let found: Vec<&serde_json::Value> = listed["tests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| {
            t["name"]
                .as_str()
                .is_some_and(|n| names.iter().any(|w| w == n))
        })
        .collect();
    let command = |t: &serde_json::Value| -> Vec<String> {
        t["command"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a.as_str().map(str::to_string))
            .collect()
    };
    let Some(first) = found.first() else {
        let name = match whole_file {
            true => file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            false => runnable.name.clone(),
        };
        return Err(format!(
            "CTest lists no test of {name}: build the project again, then debug it."
        ));
    };
    let first_command = command(first);
    let Some((program, rest)) = first_command.split_first() else {
        return Err("CTest's list of tests could not be read.".to_string());
    };
    let mut args = rest.to_vec();
    if found.len() > 1 {
        if found.iter().any(|t| command(t).first() != Some(program)) {
            return Err(
                "These tests are in more than one program: debug them one at a time.".to_string(),
            );
        }
        let names: Vec<&str> = found.iter().filter_map(|t| t["name"].as_str()).collect();
        // GoogleTest's filter, or Catch2's test name: each test's, joined.
        if let Some(at) = args.iter().position(|a| a.starts_with("--gtest_filter=")) {
            args[at] = format!("--gtest_filter={}", names.join(":"));
        } else if args.first().map(String::as_str) == first["name"].as_str() {
            args[0] = names.join(",");
        }
    }
    let cwd = first["properties"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == "WORKING_DIRECTORY")
        .and_then(|p| p["value"].as_str())
        .map_or(build, PathBuf::from);
    Ok(DebugTest {
        adapter: "native",
        program: PathBuf::from(program),
        args,
        cwd,
        launch: serde_json::Map::new(),
        start: DebugStart::Program,
    })
}

/// How a debugged test run ended, read off its runner's last words where
/// its debugger says no exit code (Delve, rdbg): Go's `PASS` or `FAIL`;
/// Minitest's `… 0 failures, 0 errors …`; RSpec's `… examples, 0
/// failures`. 0 when its tests passed, 1 when not; none when it said
/// neither — stopped before its end.
pub fn test_end(lang: &str, said: &str) -> Option<i64> {
    let count = |line: &str, word: &str| -> Option<u64> {
        let at = line.find(word)?;
        line[..at]
            .trim_end()
            .rsplit([' ', ','])
            .next()?
            .parse()
            .ok()
    };
    said.lines().rev().find_map(|line| {
        let line = line.trim();
        match lang {
            "go" => match line {
                "PASS" => Some(0),
                "FAIL" => Some(1),
                _ => None,
            },
            "ruby" if line.contains(" runs, ") || line.contains(" run, ") => {
                let failed = count(line, " failure")? + count(line, " error")?;
                Some(i64::from(failed > 0))
            }
            "ruby" if line.contains(" example") => Some(i64::from(count(line, " failure")? > 0)),
            _ => None,
        }
    })
}

/// The languages whose programs [`main_command`] runs: Kotlin's, Scala's
/// and Elixir's, which no debugger of Clew's runs.
pub fn handles_main(lang: &str) -> bool {
    matches!(lang, "kotlin" | "scala" | "elixir" | "csharp" | "zig")
}

/// The languages whose programs [`debug_main`] debugs with no launch file:
/// [`handles_main`]'s, and C's and C++'s (built by their CMake build, or a
/// file alone by the compiler, then run under lldb — or, for Run, with the
/// debugger off).
pub fn debugs_main(lang: &str) -> bool {
    handles_main(lang) || matches!(lang, "c" | "cpp")
}

/// The CMake executable target `file` is a source of, by the
/// `add_executable(name sources…)` calls of the project's CMakeLists:
/// its name and the folder its CMakeLists is in.
fn cmake_target(root: &Path, file: &Path) -> Option<(String, PathBuf)> {
    let file_name = file.file_name()?.to_string_lossy().into_owned();
    let mut dir = file.parent()?.to_path_buf();
    loop {
        let lists = dir.join("CMakeLists.txt");
        if let Some(text) = crate::statefile::read_capped(&lists, 4 << 20) {
            let mut rest = text.as_str();
            while let Some(at) = rest.find("add_executable(") {
                let after = &rest[at + "add_executable(".len()..];
                let end = after.find(')').unwrap_or(after.len());
                let words: Vec<&str> = after[..end]
                    .split_whitespace()
                    .filter(|w| !matches!(*w, "WIN32" | "MACOSX_BUNDLE" | "EXCLUDE_FROM_ALL"))
                    .collect();
                if let Some((name, sources)) = words.split_first() {
                    let listed = sources.iter().any(|s| {
                        let s = s.trim_matches('"');
                        dir.join(s) == file || s.rsplit('/').next() == Some(file_name.as_str())
                    });
                    if listed {
                        return Some((name.trim_matches('"').to_string(), dir));
                    }
                }
                rest = &after[end..];
            }
        }
        if dir == root || !dir.pop() || !dir.starts_with(root) {
            return None;
        }
    }
}

/// Whether the folder `file` is in, or one above it, is built by a build
/// of its own Clew does not read (make, Meson, Bazel): its program is the
/// one a launch file names.
fn other_c_build(root: &Path, file: &Path) -> bool {
    above(
        root,
        file,
        &[
            "Makefile",
            "makefile",
            "GNUmakefile",
            "meson.build",
            "BUILD",
            "BUILD.bazel",
        ],
    )
    .is_some()
}

/// Whether a C or C++ file has a `main` ([`main_line`]'s: `int main(` at
/// the top level, not a `domain(`).
fn has_c_main(file: &Path, lang: &str) -> bool {
    crate::statefile::read_capped(file, 4 << 20).is_some_and(|text| {
        let lines: Vec<&str> = text.lines().collect();
        main_line(lang, &lines).is_some()
    })
}

/// A C or C++ program built for a session: by its CMake build's target, or
/// the file alone by the compiler (`-g -O0`, into Clew's data directory) —
/// the build's words to `said` as they come.
fn build_c_program(
    root: &Path,
    file: &Path,
    lang: &str,
    search: &OsStr,
    said: &mut dyn FnMut(&str),
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(PathBuf, PathBuf), String> {
    let tool = |name: &str| {
        crate::lsp::store::find_in_unresolved(search, name)
            .ok_or_else(|| format!("{other} wasn't found here.", other = name))
    };
    if let Some((target, _)) = cmake_target(root, file) {
        let cmake = tool("cmake")?;
        // The project's configured build; none yet, a Debug one configured
        // in `build/`, as an editor's CMake support does.
        let top = cmake_top(root, file).unwrap_or_else(|| root.to_path_buf());
        let build = match cmake_build_dir(&top) {
            Some(build) => build,
            None => {
                let build = top.join("build");
                let mut cmd = crate::spawn::command(&cmake);
                cmd.arg("-S")
                    .arg(&top)
                    .arg("-B")
                    .arg(&build)
                    .arg("-DCMAKE_BUILD_TYPE=Debug")
                    .current_dir(&top);
                if !build_step(cmd, said, cancel)?.0 {
                    return Err("CMake could not configure the build (its output is above).".into());
                }
                build
            }
        };
        let mut cmd = crate::spawn::command(&cmake);
        cmd.arg("--build")
            .arg(&build)
            .arg("--target")
            .arg(&target)
            .current_dir(root);
        if !build_step(cmd, said, cancel)?.0 {
            return Err(format!(
                "CMake could not build {target} (its output is above)."
            ));
        }
        let program = find_program(&build, &target).ok_or_else(|| {
            format!(
                "{target} was built, but its program wasn't found in {folder}.",
                folder = build.display()
            )
        })?;
        let cwd = program.parent().unwrap_or(&build).to_path_buf();
        return Ok((program, cwd));
    }
    let compilers: &[&str] = match lang {
        "c" => &["cc", "gcc", "clang"],
        _ => &["c++", "g++", "clang++"],
    };
    let compiler = compilers
        .iter()
        .find_map(|c| crate::lsp::store::find_in_unresolved(search, c))
        .ok_or_else(|| format!("{other} wasn't found here.", other = compilers[0]))?;
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    file.hash(&mut hasher);
    let out = crate::lsp::store::data_root()
        .unwrap_or_else(std::env::temp_dir)
        .join("debug-builds")
        .join("c")
        .join(format!("{:016x}", hasher.finish()));
    std::fs::create_dir_all(&out).map_err(|e| format!("cannot make {}: {e}", out.display()))?;
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "program".into());
    let program = out.join(if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem
    });
    let mut cmd = crate::spawn::command(&compiler);
    cmd.args(["-g", "-O0"])
        .arg(file)
        .arg("-o")
        .arg(&program)
        .current_dir(file.parent().unwrap_or(root));
    if !build_step(cmd, said, cancel)?.0 {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        return Err(format!(
            "{file} did not compile (the compiler's output is above).",
            file = name
        ));
    }
    Ok((program, file.parent().unwrap_or(root).to_path_buf()))
}

/// One step of a build, run to its end, each line it writes handed to
/// `said` as it comes: whether it succeeded, and its lines (what it names
/// it built). Stopped, a step says so.
fn build_step(
    cmd: std::process::Command,
    said: &mut dyn FnMut(&str),
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(bool, Vec<String>), String> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let mut lines = Vec::new();
    let end = crate::spawn::run_streaming(
        cmd,
        |line, _| {
            said(line);
            lines.push(line.to_string());
        },
        cancel,
    )
    .map_err(|e| format!("could not run {name}: {e}"))?;
    match end {
        crate::spawn::RunEnd::Exited(code) => Ok((code == 0, lines)),
        crate::spawn::RunEnd::Stopped => Err("The build was stopped.".into()),
    }
}

/// The top CMake project `file` is in: the highest folder, from the file
/// up to the project's root, with a CMakeLists.
fn cmake_top(root: &Path, file: &Path) -> Option<PathBuf> {
    let mut top = None;
    let mut dir = file.parent()?.to_path_buf();
    while dir.starts_with(root) {
        if dir.join("CMakeLists.txt").is_file() {
            top = Some(dir.clone());
        }
        if dir == root || !dir.pop() {
            break;
        }
    }
    top
}

/// A configured CMake build (`CMakeCache.txt`) of the project at `top`:
/// a `build…` or `cmake-build-…` folder, a Debug one first, or one of
/// `out/build`'s (Visual Studio's).
fn cmake_build_dir(top: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(top)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    if let Ok(out) = std::fs::read_dir(top.join("out/build")) {
        found.extend(out.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    }
    found.retain(|p| p.join("CMakeCache.txt").is_file());
    found.sort_by_key(|p| {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        (!name.contains("debug"), name)
    });
    found.into_iter().next()
}

/// The program a build made, by its target's name, in the build folder.
fn find_program(build: &Path, name: &str) -> Option<PathBuf> {
    let wanted = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let mut todo = vec![(build.to_path_buf(), 0)];
    while let Some((dir, depth)) = todo.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skip = path
                    .file_name()
                    .is_some_and(|n| n == "CMakeFiles" || n.to_string_lossy().starts_with('.'));
                if depth < 6 && !skip {
                    todo.push((path, depth + 1));
                }
            } else if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy() == wanted)
            {
                return Some(path);
            }
        }
    }
    None
}

/// The .NET project `file` is in, and whether it makes a program — not a
/// library, nor a test project (`Microsoft.NET.Test.Sdk`).
fn dotnet_project(root: &Path, file: &Path) -> Option<(PathBuf, bool)> {
    let project = above_matching(root, file, |n| {
        n.ends_with(".csproj") || n.ends_with(".fsproj") || n.ends_with(".vbproj")
    })?;
    let text = crate::statefile::read_capped(&project, 1 << 20).unwrap_or_default();
    let tests = text.contains("Microsoft.NET.Test.Sdk") || text.contains("<IsTestProject>true");
    let program = text.contains("<OutputType>Exe")
        || text.contains("<OutputType>WinExe")
        || text.contains("Microsoft.NET.Sdk.Web")
        || text.contains("Microsoft.NET.Sdk.Worker");
    Some((project, program && !tests))
}

/// The command that runs `file`, a program in `lang` under `root`, as its
/// project runs one: a Kotlin script by `kotlinc -script`, a Kotlin
/// program by its Gradle build's `run`; Scala by sbt's `run` in an sbt
/// build, else by scala-cli (`scala`, since Scala 3.5); Elixir by `mix run`
/// in a Mix project (a script by it, with the project loaded), else by
/// `elixir`.
pub fn main_command(
    root: &Path,
    file: &Path,
    lang: &str,
    search: &OsStr,
) -> Result<JsCommand, String> {
    let tool = |name: &str| {
        crate::lsp::store::find_in_unresolved(search, name)
            .ok_or_else(|| format!("{other} wasn't found here.", other = name))
    };
    let command = |program: PathBuf, args: Vec<String>, cwd: &Path| JsCommand {
        program,
        args,
        cwd: cwd.to_path_buf(),
        env: Vec::new(),
    };
    let name = file.to_string_lossy().into_owned();
    match lang {
        "kotlin" if name.ends_with(".kts") => {
            let dir = file.parent().unwrap_or(root);
            Ok(command(tool("kotlinc")?, vec!["-script".into(), name], dir))
        }
        "kotlin" => {
            let dir = above(
                root,
                file,
                &[
                    "build.gradle.kts",
                    "build.gradle",
                    "settings.gradle.kts",
                    "settings.gradle",
                ],
            )
            .ok_or_else(|| no_program("build.gradle.kts", file))?;
            let gradle = wrapper(root, &dir, "gradlew").map_or_else(|| tool("gradle"), Ok)?;
            Ok(command(
                gradle,
                vec![
                    "-p".into(),
                    dir.to_string_lossy().into_owned(),
                    "run".into(),
                ],
                &dir,
            ))
        }
        "scala" => {
            if let Some(dir) = above(root, file, &["build.sbt"]) {
                // The file's own program (`runMain shop.Main`): `run` asks
                // which, of a build with more than one, and has no one to
                // answer.
                let source = crate::statefile::read_capped(file, 4 << 20).unwrap_or_default();
                let run = match scala_main_class(&source) {
                    Some(class) => format!("runMain {class}"),
                    None => "run".into(),
                };
                return Ok(command(tool("sbt")?, vec![run], &dir));
            }
            // A Mill module's program: `mill foo.runMain shop.Main`, the
            // module read off the file's place (`foo/src/…`).
            if let Some(dir) = above(root, file, &["build.mill", "build.sc"]) {
                let source = crate::statefile::read_capped(file, 4 << 20).unwrap_or_default();
                let module = mill_module(&dir, file).ok_or_else(|| no_main(file))?;
                let mut args = vec![format!("{module}.runMain")];
                args.extend(scala_main_class(&source));
                if args.len() == 1 {
                    args = vec![format!("{module}.run")];
                }
                return Ok(command(tool("mill")?, args, &dir));
            }
            let dir = file.parent().unwrap_or(root);
            let cli = tool("scala-cli").or_else(|_| tool("scala"))?;
            Ok(command(cli, vec!["run".into(), name], dir))
        }
        "elixir" => match above(root, file, &["mix.exs"]) {
            Some(project) => {
                let mut args = vec!["run".to_string()];
                if name.ends_with(".exs") {
                    args.push(rel_slash(&project, file));
                }
                Ok(command(tool("mix")?, args, &project))
            }
            None => {
                let dir = file.parent().unwrap_or(root);
                Ok(command(tool("elixir")?, vec![name], dir))
            }
        },
        // The project's program, as `dotnet run` runs it.
        "csharp" => {
            let (project, program) =
                dotnet_project(root, file).ok_or_else(|| no_program(".csproj", file))?;
            if !program {
                return Err(not_a_program(&project));
            }
            let dir = project.parent().unwrap_or(root).to_path_buf();
            Ok(command(
                tool("dotnet")?,
                vec![
                    "run".into(),
                    "--project".into(),
                    project.to_string_lossy().into_owned(),
                ],
                &dir,
            ))
        }
        // A build.zig's program (`zig build run`), else the file alone —
        // when it has a `main` to run.
        "zig" => match above(root, file, &["build.zig"]) {
            Some(dir) => Ok(command(
                tool("zig")?,
                vec!["build".into(), "run".into()],
                &dir,
            )),
            None if !has_zig_main(file) => Err(no_main(file)),
            None => {
                let dir = file.parent().unwrap_or(root);
                Ok(command(tool("zig")?, vec!["run".into(), name], dir))
            }
        },
        _ => Err(format!("{rel} has no program to run", rel = lang)),
    }
}

/// Whether a Zig file has a `main` to run.
fn has_zig_main(file: &Path) -> bool {
    crate::statefile::read_capped(file, 4 << 20)
        .is_some_and(|text| text.lines().any(|l| l.contains("fn main(")))
}

/// A file with no `main`: its tests are what runs.
fn no_main(file: &Path) -> String {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!(
        "{name} has no main function to run: run its tests with Run Test, or open the program's own file."
    )
}

/// A .NET project that makes no program: a library, or tests.
fn not_a_program(project: &Path) -> String {
    let name = project
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{name} is a library or a test project, so it has no program to run.")
}

/// "No `manifest` was found above `file`": the build that runs it.
fn no_program(manifest: &str, file: &Path) -> String {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("No {manifest} was found above {name}, so it has no build to run it.")
}

/// "No `manifest` was found above `file`": where its tests run from.
fn no_manifest(manifest: &str, file: &Path) -> String {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("No {manifest} was found above {name}, so its tests have no project to run in.")
}

/// The folder at or above `file`'s, up to `root`, holding one of `names`.
fn above(root: &Path, file: &Path, names: &[&str]) -> Option<PathBuf> {
    let mut dir = file.parent()?.to_path_buf();
    loop {
        if names.iter().any(|n| dir.join(n).is_file()) {
            return Some(dir);
        }
        if dir == root || !dir.pop() || !dir.starts_with(root) {
            return None;
        }
    }
}

/// The first file at or above `file`'s folder, up to `root`, whose name
/// `pick` takes.
fn above_matching(root: &Path, file: &Path, pick: impl Fn(&str) -> bool) -> Option<PathBuf> {
    let mut dir = file.parent()?.to_path_buf();
    loop {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.file_name().is_some_and(|n| pick(&n.to_string_lossy())))
            .collect();
        found.sort();
        if let Some(first) = found.into_iter().next() {
            return Some(first);
        }
        if dir == root || !dir.pop() || !dir.starts_with(root) {
            return None;
        }
    }
}

/// A build's wrapper script (`gradlew`, `mvnw`) in `dir` or a folder above
/// it in the project: what the project runs its build with.
fn wrapper(root: &Path, dir: &Path, name: &str) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        format!("{name}.bat")
    } else {
        name.to_string()
    };
    dir.ancestors()
        .take_while(|d| d.starts_with(root))
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
}

/// `path` from `base`, with `/`.
fn rel_slash(base: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(base).unwrap_or(path);
    let text = rel.to_string_lossy().replace('\\', "/");
    if text.is_empty() { ".".into() } else { text }
}

/// A JVM test's class by its full name, and its method: the file's
/// `package` and the class the test is in (`com.acme.CartTest`,
/// `adds`); a class's group, the class alone. A suite's test called by
/// name (`test("adds")`, ScalaTest's and MUnit's) is in the class around
/// it.
fn jvm_test_name(source: &str, runnable: &Runnable) -> Option<(String, Option<String>)> {
    let lines: Vec<&str> = source.lines().collect();
    let package = lines.iter().find_map(|l| {
        let t = l.trim();
        t.strip_prefix("package ")
            .map(|p| p.trim().trim_end_matches(';').trim().to_string())
    });
    let (class, method) = match runnable.kind {
        RunnableKind::Tests if runnable.name.is_empty() => return None,
        RunnableKind::Tests
            if !runnable.scopes.is_empty() || is_class_line(&lines, runnable.line) =>
        {
            (runnable.name.clone(), None)
        }
        // A JUnit test's scope is its class; a `describe`'s is no class,
        // and the test's class is the one around it.
        _ => match runnable.scopes.first().filter(|scope| {
            lines
                .iter()
                .any(|l| class_name(l).as_deref() == Some(scope.as_str()))
        }) {
            Some(class) if !is_class_line(&lines, runnable.line) => {
                (class.clone(), Some(runnable.name.clone()))
            }
            _ => (
                enclosing_class(&lines, runnable.line)?,
                Some(runnable.name.clone()),
            ),
        },
    };
    let full = match package {
        Some(package) if !package.is_empty() => format!("{package}.{class}"),
        _ => class,
    };
    Some((full, method))
}

/// The class a Scala file's program runs as, by its full name: the
/// object with a `def main(args` or that `extends App`, or a `@main`
/// method's own name (Scala 3 makes a class of it), in the file's package.
fn scala_main_class(source: &str) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let package: Vec<String> = lines
        .iter()
        .filter_map(|l| l.trim().strip_prefix("package "))
        .map(|p| p.trim().trim_end_matches(':').trim().to_string())
        .collect();
    let at = main_line("scala", &lines)?;
    let line = lines[at - 1].trim();
    let name = if let Some(rest) = line
        .strip_prefix("@main def ")
        .or_else(|| line.strip_prefix("@main\tdef "))
    {
        rest.split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .next()?
            .to_string()
    } else if line.starts_with("object ") {
        class_name(line)?
    } else {
        // `def main(args…` in the object around it.
        enclosing_class(&lines, at)?
    };
    let package = package.join(".");
    Some(match package.is_empty() {
        true => name,
        false => format!("{package}.{name}"),
    })
}

/// The words a JVM test framework picks one test of a class by, after
/// sbt's or scala-cli's `--`: ScalaTest's `-z name` (its name's text),
/// MUnit's full name (`acme.CartSuite.adds`, spaces and all), JUnit's
/// interface's `--tests=method`. None for a group, or a framework that
/// has none.
fn jvm_test_filter(
    source: &str,
    runnable: &Runnable,
    class: Option<&(String, Option<String>)>,
) -> Option<Vec<String>> {
    if runnable.kind != RunnableKind::Test {
        return None;
    }
    let (class, name) = class?;
    if source.contains("org.scalatest") {
        return Some(vec!["-z".into(), runnable.name.clone()]);
    }
    let name = name.as_ref()?;
    if source.contains("munit.") {
        return Some(vec![format!("{class}.{name}")]);
    }
    source
        .contains("org.junit")
        .then(|| vec![format!("--tests={name}")])
}

/// The test classes of a JVM file, by their full names: what running the
/// file's tests runs.
fn file_test_classes(lang: &str, source: &str) -> Vec<String> {
    let mut classes: Vec<String> = Vec::new();
    for runnable in tests(lang, "", source) {
        if runnable.kind == RunnableKind::Main
            || (runnable.kind == RunnableKind::Tests && runnable.name.is_empty())
        {
            continue;
        }
        if let Some((class, _)) = jvm_test_name(source, &runnable)
            && !classes.contains(&class)
        {
            classes.push(class);
        }
    }
    classes
}

/// Whether line `line1` declares a class or an object (a JUnit class's
/// group is its declaration).
fn is_class_line(lines: &[&str], line1: usize) -> bool {
    lines
        .get(line1.wrapping_sub(1))
        .is_some_and(|l| class_name(l).is_some())
}

/// The class or object a line declares, by name.
fn class_name(line: &str) -> Option<String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let at = words
        .iter()
        .position(|w| matches!(*w, "class" | "object"))?;
    let name: String = words
        .get(at + 1)?
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The class around line `line1`: the nearest above it, indented less.
fn enclosing_class(lines: &[&str], line1: usize) -> Option<String> {
    let own = indent(lines.get(line1.checked_sub(1)?)?);
    (0..line1 - 1)
        .rev()
        .filter(|&i| indent(lines[i]) < own)
        .find_map(|i| class_name(lines[i]))
}

/// The namespace a C# file's code is in: its file-scoped or first
/// block-bodied `namespace`.
fn csharp_namespace(source: &str) -> Option<String> {
    // A byte-order mark is no part of the first line's text.
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    source.lines().find_map(|l| {
        let name = l.trim().strip_prefix("namespace ")?;
        let name = name
            .trim()
            .trim_end_matches(';')
            .trim_end_matches('{')
            .trim();
        (!name.is_empty()).then(|| name.to_string())
    })
}

/// The CMake build folder CTest runs a project's tests from: one under the
/// project (`build`, `cmake-build-debug`, `out/build/x64-Debug`) holding a
/// `CTestTestfile.cmake`.
fn ctest_dir(root: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name().is_some_and(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with("build") || n.starts_with("cmake-build")
                })
        })
        .collect();
    if let Ok(out) = std::fs::read_dir(root.join("out/build")) {
        candidates.extend(out.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    }
    candidates.sort();
    candidates
        .into_iter()
        .find(|d| d.join("CTestTestfile.cmake").is_file())
}

/// `text` as a regular expression matching it alone.
fn regex_literal(text: &str) -> String {
    let mut out = String::from("^");
    for c in text.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('$');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(found: &[Runnable]) -> Vec<(RunnableKind, String, usize, (usize, usize))> {
        found
            .iter()
            .map(|r| (r.kind, r.name.clone(), r.line, r.span))
            .collect()
    }

    /// Each language's tests by its convention, each spanning its body —
    /// a file's every test a group, first.
    /// A file saved with a byte-order mark before its `namespace` line is
    /// still in that namespace: the test's full name has it.
    #[test]
    fn a_csharp_namespace_after_a_byte_order_mark() {
        assert_eq!(
            csharp_namespace("\u{feff}namespace Shop.Tests;\n\npublic class CartTests {}\n")
                .as_deref(),
            Some("Shop.Tests")
        );
    }

    #[test]
    fn each_languages_tests_are_found() {
        let go = "package cart\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {\n\tt.Run(\"x\", nil)\n}\n\nfunc helper() {}\n\nfunc BenchmarkAdd(b *testing.B) {\n}\n";
        let found = tests("go", "cart/cart_test.go", go);
        assert_eq!(
            names(&found)[1..],
            [
                (RunnableKind::Test, "TestAdd".into(), 5, (5, 7)),
                (RunnableKind::Test, "BenchmarkAdd".into(), 11, (11, 12)),
            ]
        );
        assert_eq!(found[0].kind, RunnableKind::Tests);
        assert!(tests("go", "cart/cart.go", go).is_empty(), "no test file");

        let kt = "package com.acme\n\nimport org.junit.jupiter.api.Test\n\nclass CartTest {\n    @Test\n    fun `adds items`() {\n        check(1 == 1)\n    }\n\n    fun helper() {}\n\n    @ParameterizedTest\n    @ValueSource(ints = [1])\n    fun takes(n: Int) {}\n}\n";
        let found = tests("kotlin", "src/test/kotlin/com/acme/CartTest.kt", kt);
        assert_eq!(
            names(&found)[1..],
            [
                (RunnableKind::Tests, "CartTest".into(), 5, (5, 16)),
                (RunnableKind::Test, "adds items".into(), 7, (7, 9)),
                (RunnableKind::Test, "takes".into(), 15, (15, 15)),
            ]
        );
        let java = "package com.acme;\n\nclass CartTest {\n  @Test void adds() {\n  }\n  void helper() {}\n}\n";
        let found = tests("java", "CartTest.java", java);
        assert_eq!(
            names(&found)[1..],
            [
                (RunnableKind::Tests, "CartTest".into(), 3, (3, 7)),
                (RunnableKind::Test, "adds".into(), 4, (4, 5)),
            ]
        );

        let cs = "namespace Shop.Tests;\n\npublic class CartTests\n{\n    [Fact]\n    public void Adds()\n    {\n    }\n\n    [Theory]\n    [InlineData(1)]\n    public void Takes(int n) { }\n\n    public void Helper() { }\n}\n";
        let found = tests("csharp", "CartTests.cs", cs);
        let got: Vec<String> = found.iter().map(|r| r.name.clone()).collect();
        assert_eq!(got, ["", "CartTests", "Adds", "Takes"]);

        let dart = "void main() {\n  group('Cart', () {\n    test('adds', () {\n      expect(1, 1);\n    });\n  });\n  testWidgets(\"renders\", (t) async {});\n}\n";
        let found = tests("dart", "test/cart_test.dart", dart);
        assert_eq!(
            names(&found)[1..],
            [
                (RunnableKind::Tests, "Cart".into(), 2, (2, 6)),
                (RunnableKind::Test, "adds".into(), 3, (3, 5)),
                (RunnableKind::Test, "renders".into(), 7, (7, 7)),
            ]
        );
        assert_eq!(found[2].scopes, ["Cart"]);

        let rspec = "RSpec.describe Cart do\n  context 'empty' do\n    it 'has no items' do\n      expect(1).to eq(1)\n    end\n  end\nend\n";
        let found = tests("ruby", "spec/cart_spec.rb", rspec);
        assert_eq!(
            names(&found)[1..],
            [
                (RunnableKind::Tests, "empty".into(), 2, (2, 6)),
                (RunnableKind::Test, "has no items".into(), 3, (3, 5)),
            ]
        );
        let minitest = "class CartTest < Minitest::Test\n  def test_adds\n    assert true\n  end\n\n  test \"removes items\" do\n  end\nend\n";
        let got: Vec<String> = tests("ruby", "test/cart_test.rb", minitest)
            .iter()
            .map(|r| r.name.clone())
            .collect();
        assert_eq!(got, ["", "test_adds", "test_removes_items"]);

        let zig = "const std = @import(\"std\");\n\ntest \"adds\" {\n    try std.testing.expect(true);\n}\n\ntest add {\n}\n";
        let got: Vec<String> = tests("zig", "src/cart.zig", zig)
            .iter()
            .map(|r| r.name.clone())
            .collect();
        assert_eq!(got, ["", "adds", "add"]);

        let erl = "-module(cart_tests).\n-include_lib(\"eunit/include/eunit.hrl\").\n\nadd_test() ->\n    ?assert(true).\n\nmany_test_() ->\n    [?_assert(true)].\n\nhelper() -> ok.\n";
        let got: Vec<String> = tests("erlang", "test/cart_tests.erl", erl)
            .iter()
            .map(|r| r.name.clone())
            .collect();
        assert_eq!(got, ["", "add_test", "many_test_"]);
        let suite = "-module(cart_SUITE).\n-export([all/0, adds/1]).\n\nall() -> [adds].\n\ninit_per_suite(Config) -> Config.\n\nadds(_Config) ->\n    ok.\n";
        let got: Vec<String> = tests("erlang", "test/cart_SUITE.erl", suite)
            .iter()
            .map(|r| r.name.clone())
            .collect();
        assert_eq!(got, ["", "adds"]);

        let ex = "defmodule CartTest do\n  use ExUnit.Case\n\n  describe \"add/2\" do\n    test \"adds\" do\n      assert 1 == 1\n    end\n  end\n\n  test \"empty\", %{cart: cart} do\n  end\nend\n";
        let found = tests("elixir", "test/cart_test.exs", ex);
        assert_eq!(
            names(&found)[1..],
            [
                (RunnableKind::Tests, "add/2".into(), 4, (4, 8)),
                (RunnableKind::Test, "adds".into(), 5, (5, 7)),
                (RunnableKind::Test, "empty".into(), 10, (10, 11)),
            ]
        );

        // hspec's group bound to `spec`, as it is written most: a group of
        // its own, around its tests.
        let hs =
            "spec :: Spec\nspec = describe \"Cart\" $ do\n  it \"adds\" $ do\n    1 `shouldBe` 1\n";
        let found = tests("haskell", "test/CartSpec.hs", hs);
        let got: Vec<(String, usize, Vec<String>)> = found
            .iter()
            .map(|r| (r.name.clone(), r.line, r.scopes.clone()))
            .collect();
        assert_eq!(
            got,
            [
                (String::new(), 1, vec![]),
                ("Cart".into(), 2, vec![]),
                ("adds".into(), 3, vec!["Cart".to_string()]),
            ]
        );

        let cpp = "#include <gtest/gtest.h>\n\nTEST(Cart, Adds) {\n  EXPECT_EQ(1, 1);\n}\n\nTEST_CASE(\"cart empties\") {\n}\n";
        let got: Vec<String> = tests("cpp", "tests/cart_test.cpp", cpp)
            .iter()
            .map(|r| r.name.clone())
            .collect();
        assert_eq!(got, ["", "Cart.Adds", "cart empties"]);

        let scala = "package acme\n\nclass CartSuite extends munit.FunSuite {\n  test(\"adds\") {\n    assert(true)\n  }\n}\n";
        let got: Vec<String> = tests("scala", "src/test/scala/acme/CartSuite.scala", scala)
            .iter()
            .map(|r| r.name.clone())
            .collect();
        assert_eq!(got, ["", "CartSuite", "adds"], "the suite, a group");
    }

    /// Each test runs as its project runs tests: Go's package by its
    /// module, a JUnit test through Gradle's wrapper by its class and
    /// method, Maven's alike, C#'s by its full name, Dart's by name (Flutter
    /// for a Flutter package), RSpec by its line, ExUnit by its line,
    /// rebar3 by its module and function.
    #[test]
    fn each_test_runs_by_its_projects_runner() {
        let tmp = crate::testutil::TempDir::new("langtests-commands");
        let root = tmp.path().to_path_buf();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        };
        let bin = root.join("bin");
        for name in [
            "go", "dotnet", "dart", "flutter", "bundle", "mix", "rebar3", "mvn", "sbt",
        ] {
            let path = bin.join(name);
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let search = bin.as_os_str();
        let run = |lang: &str, file: &Path, line: usize| {
            let source = std::fs::read_to_string(file).unwrap();
            let rel = file
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let found = tests(lang, &rel, &source);
            let r = crate::runnables::runnable_at(&found, line)
                .unwrap_or_else(|| panic!("{lang} line {line}: {found:?}"))
                .clone();
            test_command(&root, file, lang, &source, &r, search)
                .unwrap_or_else(|e| panic!("{lang}: {e}"))
        };

        write("svc/go.mod", "module acme/svc\n");
        let go = write(
            "svc/cart/cart_test.go",
            "package cart\n\nfunc TestAdd(t *testing.T) {\n}\n",
        );
        let c = run("go", &go, 3);
        assert_eq!(c.args, ["test", "-v", "-run", "^TestAdd$", "./cart"]);
        assert_eq!(c.cwd, root.join("svc"));
        // The file's tests, not its package's every one.
        let two = write(
            "svc/cart/two_test.go",
            "package cart\n\nfunc TestA(t *testing.T) {\n}\n\nfunc BenchmarkB(b *testing.B) {\n}\n\nfunc ExampleC() {\n}\n",
        );
        let c = run("go", &two, 1);
        assert_eq!(
            c.args,
            ["test", "-v", "-run", "^(TestA|ExampleC)$", "./cart"]
        );

        write("app/settings.gradle.kts", "");
        write("app/gradlew", "#!/bin/sh\n");
        write("app/lib/build.gradle.kts", "");
        let kt = write(
            "app/lib/src/test/kotlin/com/acme/CartTest.kt",
            "package com.acme\n\nclass CartTest {\n    @Test\n    fun adds() {\n    }\n}\n",
        );
        let c = run("kotlin", &kt, 5);
        assert_eq!(c.program, root.join("app/gradlew"));
        assert_eq!(
            c.args,
            [
                "-p".to_string(),
                root.join("app/lib").to_string_lossy().into_owned(),
                "cleanTest".into(),
                "test".into(),
                "--tests".into(),
                "com.acme.CartTest.adds".into(),
                "--init-script".into(),
                gradle_test_script().unwrap().to_string_lossy().into_owned()
            ]
        );
        write("mvn/pom.xml", "<project/>\n");
        let java = write(
            "mvn/src/test/java/com/acme/CartTest.java",
            "package com.acme;\nclass CartTest {\n  @Test\n  void adds() {}\n}\n",
        );
        let c = run("java", &java, 4);
        assert_eq!(c.args.last().unwrap(), "-Dtest=com.acme.CartTest#adds");

        write("cs/Shop.Tests.csproj", "<Project/>\n");
        let cs = write(
            "cs/CartTests.cs",
            "namespace Shop.Tests;\n\npublic class CartTests\n{\n    [Fact]\n    public void Adds()\n    {\n    }\n}\n",
        );
        let c = run("csharp", &cs, 6);
        assert_eq!(
            c.args[2..],
            ["--filter", "FullyQualifiedName~Shop.Tests.CartTests.Adds"]
        );

        write(
            "fl/pubspec.yaml",
            "dependencies:\n  flutter:\n    sdk: flutter\n",
        );
        let dart = write(
            "fl/test/a_test.dart",
            "void main() {\n  test('adds', () {});\n}\n",
        );
        let c = run("dart", &dart, 2);
        assert_eq!(c.program, bin.join("flutter"));
        assert_eq!(c.args, ["test", "test/a_test.dart", "--plain-name", "adds"]);

        write("rb/Gemfile", "");
        let spec = write(
            "rb/spec/cart_spec.rb",
            "describe Cart do\n  it 'adds' do\n  end\nend\n",
        );
        let c = run("ruby", &spec, 2);
        assert_eq!(c.args, ["exec", "rspec", "spec/cart_spec.rb:2"]);

        write("ex/mix.exs", "");
        let ex = write(
            "ex/test/cart_test.exs",
            "defmodule CartTest do\n  test \"adds\" do\n  end\nend\n",
        );
        let c = run("elixir", &ex, 2);
        assert_eq!(c.args, ["test", "test/cart_test.exs:2"]);
        assert_eq!(c.cwd, root.join("ex"));

        write("erl/rebar.config", "");
        let erl = write(
            "erl/test/cart_tests.erl",
            "-module(cart_tests).\nadd_test() ->\n    ok.\n",
        );
        let c = run("erlang", &erl, 2);
        assert_eq!(c.args, ["eunit", "--test=cart_tests:add_test"]);

        write("sc/build.sbt", "");
        let scala = write(
            "sc/src/test/scala/acme/CartSuite.scala",
            "package acme\n\nclass CartSuite extends munit.FunSuite {\n  test(\"adds\") {\n  }\n}\n",
        );
        let c = run("scala", &scala, 4);
        assert_eq!(
            c.args,
            ["testOnly acme.CartSuite -- \"acme.CartSuite.adds\""],
            "MUnit's: by its full name"
        );
        let c = run("scala", &scala, 3);
        assert_eq!(c.args, ["testOnly acme.CartSuite"], "the suite: its class");
        let c = run("scala", &scala, 1);
        assert_eq!(c.args, ["testOnly acme.CartSuite"], "the file: its suites");
        let scalatest = write(
            "sc/src/test/scala/acme/CartSpec.scala",
            "package acme\n\nimport org.scalatest.funsuite.AnyFunSuite\n\nclass CartSpec extends AnyFunSuite {\n  test(\"adds\") {\n  }\n}\n",
        );
        let c = run("scala", &scalatest, 6);
        assert_eq!(c.args, ["testOnly acme.CartSpec -- -z \"adds\""]);

        // Programs: Kotlin's by its Gradle build, a script by kotlinc;
        // Scala's by sbt; Elixir's by mix in a Mix project, by elixir
        // outside one.
        for name in ["kotlinc", "gradle", "elixir"] {
            let path = bin.join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let main = |lang: &str, file: &Path| main_command(&root, file, lang, search).unwrap();
        let app = write(
            "app/lib/src/main/kotlin/com/acme/Main.kt",
            "fun main() {}\n",
        );
        let c = main("kotlin", &app);
        assert_eq!(c.program, root.join("app/gradlew"));
        assert_eq!(c.args.last().unwrap(), "run");
        let script = write("tools/build.main.kts", "println(1)\n");
        let c = main("kotlin", &script);
        assert_eq!(
            (c.program, c.args[0].as_str()),
            (bin.join("kotlinc"), "-script")
        );
        let c = main("scala", &scala);
        assert_eq!(
            (c.program, c.args.clone()),
            (bin.join("sbt"), vec!["run".to_string()])
        );
        let seed = write("ex/priv/seed.exs", "IO.puts(1)\n");
        let c = main("elixir", &seed);
        assert_eq!(
            (c.args.clone(), c.cwd),
            (
                vec!["run".to_string(), "priv/seed.exs".into()],
                root.join("ex")
            )
        );
        let loose = write("loose/hello.exs", "IO.puts(1)\n");
        assert_eq!(main("elixir", &loose).program, bin.join("elixir"));
    }

    /// Each language's program start, marked on its own line, beside the
    /// file's tests; none where a file only defines things.
    #[test]
    fn a_programs_start_is_marked_in_each_language() {
        let main = |lang: &str, rel: &str, source: &str| {
            tests(lang, rel, source)
                .into_iter()
                .find(|r| r.kind == RunnableKind::Main)
                .map(|r| (r.line, r.span))
        };
        assert_eq!(
            main("go", "main.go", "package main\n\nfunc main() {\n}\n"),
            Some((3, (3, 3)))
        );
        assert_eq!(
            main("go", "util.go", "package util\n\nfunc main() {\n}\n"),
            None
        );
        assert_eq!(
            main(
                "c",
                "hello.c",
                "#include <stdio.h>\nint main(void) {\n  return 0;\n}\n"
            ),
            Some((2, (2, 2)))
        );
        assert_eq!(
            main("cpp", "lib.cpp", "int add(int a) { return a; }\n"),
            None
        );
        assert_eq!(
            main(
                "java",
                "App.java",
                "class App {\n    public static void main(String[] args) {\n    }\n}\n"
            ),
            Some((2, (2, 2)))
        );
        assert_eq!(
            main("kotlin", "Main.kt", "package a\n\nfun main() {\n}\n"),
            Some((3, (3, 3)))
        );
        assert_eq!(
            main("kotlin", "Cart.kt", "class Cart {\n    fun main() {}\n}\n"),
            None
        );
        assert_eq!(
            main(
                "scala",
                "Hello.scala",
                "@main def hello(): Unit =\n  println(1)\n"
            ),
            Some((1, (1, 1)))
        );
        assert_eq!(
            main(
                "csharp",
                "Program.cs",
                "class Program {\n    static async Task<int> Main(string[] args) {\n    }\n}\n"
            ),
            Some((2, (2, 2)))
        );
        assert_eq!(
            main(
                "dart",
                "bin/app.dart",
                "import 'x.dart';\n\nvoid main(List<String> args) {\n}\n"
            ),
            Some((3, (3, 3)))
        );
        assert_eq!(
            main(
                "zig",
                "main.zig",
                "const std = @import(\"std\");\npub fn main() !void {\n}\n"
            ),
            Some((2, (2, 2)))
        );
        assert_eq!(
            main(
                "haskell",
                "app/Main.hs",
                "module Main where\n\nmain :: IO ()\nmain = pure ()\n"
            ),
            Some((3, (3, 3)))
        );
        assert_eq!(
            main(
                "haskell",
                "src/Lib.hs",
                "module Lib where\n\nmain :: IO ()\nmain = pure ()\n"
            ),
            None
        );
        assert_eq!(
            main(
                "erlang",
                "tool.erl",
                "-module(tool).\n-export([main/1]).\n\nmain(Args) ->\n    ok.\n"
            ),
            Some((4, (4, 4)))
        );
        assert_eq!(
            main(
                "ruby",
                "tool.rb",
                "def run; end\n\nif __FILE__ == $0\n  run\nend\n"
            ),
            Some((3, (3, 3)))
        );
        // Scala 2's App object; a Kotlin object's `@JvmStatic` main; a C#
        // program's top-level statements, not a file of types.
        assert_eq!(
            main(
                "scala",
                "Shop.scala",
                "package shop\n\nobject Shop extends App {\n  println(1)\n}\n"
            )
            .map(|m| m.0),
            Some(3)
        );
        assert_eq!(
            main(
                "kotlin",
                "App.kt",
                "object App {\n    @JvmStatic\n    fun main(args: Array<String>) {}\n}\n"
            )
            .map(|m| m.0),
            Some(3)
        );
        assert_eq!(
            main(
                "csharp",
                "Program.cs",
                "// a tool\nusing System;\n\nConsole.WriteLine(\"hi\");\n"
            )
            .map(|m| m.0),
            Some(4)
        );
        assert_eq!(
            main(
                "csharp",
                "Cart.cs",
                "using System;\n\nnamespace Shop;\n\npublic class Cart {}\n"
            ),
            None
        );
        assert_eq!(
            main(
                "csharp",
                "Point.cs",
                "[Serializable]\npublic record Point(int X);\n"
            ),
            None
        );
        // Beside a Zig file's tests, the cursor in them still runs them.
        let both = tests(
            "zig",
            "main.zig",
            "pub fn main() void {\n}\n\ntest \"adds\" {\n}\n",
        );
        assert_eq!(
            crate::runnables::runnable_at(&both, 5).map(|r| r.kind),
            Some(RunnableKind::Test)
        );
        assert_eq!(
            crate::runnables::runnable_at(&both, 1).map(|r| r.kind),
            Some(RunnableKind::Main)
        );
    }

    /// A debugged test's end, by its runner's last words.
    #[test]
    fn a_tests_end_is_read_off_its_runners_last_words() {
        assert_eq!(
            test_end("go", "=== RUN   TestA\n--- PASS: TestA (0.00s)\nPASS\n"),
            Some(0)
        );
        assert_eq!(test_end("go", "--- FAIL: TestB (0.00s)\nFAIL\n"), Some(1));
        assert_eq!(
            test_end("go", "=== RUN   TestA\n"),
            None,
            "stopped before its end"
        );
        assert_eq!(
            test_end(
                "ruby",
                "Finished in 0.1s.\n\n1 runs, 1 assertions, 0 failures, 0 errors, 0 skips\n"
            ),
            Some(0)
        );
        assert_eq!(
            test_end(
                "ruby",
                "2 runs, 2 assertions, 0 failures, 1 errors, 0 skips\n"
            ),
            Some(1)
        );
        assert_eq!(test_end("ruby", "3 examples, 1 failure\n"), Some(1));
        assert_eq!(
            test_end("ruby", "1 example, 0 failures, 1 pending\n"),
            Some(0)
        );
        assert_eq!(test_end("ruby", "# Running:\n"), None);
        assert_eq!(
            test_end("java", "BUILD SUCCESSFUL\n"),
            None,
            "its build says its code"
        );
    }

    /// Each debugged test runs as Run Test runs it, under its language's
    /// debugger: Go's by Delve's test mode in its package's folder, in the
    /// test binary's words; Ruby's command under rdbg; Dart's by the test
    /// adapter, Flutter's by Flutter's; a Gradle, Maven or sbt build with
    /// the test JVM under the agent; none for a build that forks its tests
    /// out of reach, nor for languages whose tests no debugger runs.
    #[test]
    fn each_debugged_test_runs_under_its_languages_debugger() {
        // Gradle's init script is written to the data directory: a test's.
        let _data = crate::testutil::DataDir::new("langtests-debug-data");
        let tmp = crate::testutil::TempDir::new("langtests-debug");
        let root = tmp.path().to_path_buf();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        };
        let bin = root.join("bin");
        for name in [
            "go",
            "dart",
            "flutter",
            "bundle",
            "mvn",
            "sbt",
            "gradle",
            "mix",
            "kotlinc",
            "scala-cli",
            "mill",
            "dotnet",
            "cabal",
        ] {
            let path = write(&format!("bin/{name}"), "#!/bin/sh\n");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let search = bin.as_os_str();
        let choices = clew_protocol::LanguageChoices::default();
        let debug = |lang: &str, file: &Path, line: usize| {
            let source = std::fs::read_to_string(file).unwrap();
            let rel = file
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let found = tests(lang, &rel, &source);
            let r = crate::runnables::runnable_at(&found, line).unwrap().clone();
            debug_test(&root, file, lang, &source, &r, search, &choices)
                .unwrap_or_else(|e| panic!("{lang}: {e}"))
        };

        write("svc/go.mod", "module acme/svc\n");
        let go = write(
            "svc/cart/cart_test.go",
            "package cart\n\nfunc TestAdd(t *testing.T) {\n}\n\nfunc BenchmarkSum(b *testing.B) {\n}\n",
        );
        assert_eq!(test_debugger(&root, &go, "go"), Some("go"));
        let d = debug("go", &go, 3);
        assert_eq!(d.adapter, "go");
        assert_eq!(d.program, root.join("svc/cart"));
        assert_eq!(d.cwd, root.join("svc/cart"));
        assert_eq!(d.args, ["-test.v", "-test.run", "^TestAdd$"]);
        assert_eq!(d.launch["mode"], "test");
        let d = debug("go", &go, 6);
        assert_eq!(
            d.args,
            [
                "-test.v",
                "-test.run",
                "^$",
                "-test.bench",
                "^BenchmarkSum$"
            ]
        );

        write("rb/Gemfile", "");
        let spec = write(
            "rb/spec/cart_spec.rb",
            "describe Cart do\n  it 'adds' do\n  end\nend\n",
        );
        let d = debug("ruby", &spec, 2);
        assert_eq!((d.adapter, &d.start), ("ruby", &DebugStart::RubyCommand));
        assert_eq!(d.program, bin.join("bundle"));
        assert_eq!(d.args, ["exec", "rspec", "spec/cart_spec.rb:2"]);

        write(
            "fl/pubspec.yaml",
            "dependencies:\n  flutter:\n    sdk: flutter\n",
        );
        let dart = write(
            "fl/test/a_test.dart",
            "void main() {\n  test('adds', () {});\n}\n",
        );
        let d = debug("dart", &dart, 2);
        assert_eq!(d.program, dart);
        assert_eq!(d.args, ["--plain-name", "adds"]);
        assert_eq!(
            d.start,
            DebugStart::DartTests {
                flutter: Some(bin.join("flutter"))
            }
        );

        write("gr/build.gradle.kts", "");
        let kt = write(
            "gr/src/test/kotlin/acme/CartTest.kt",
            "package acme\n\nclass CartTest {\n    @Test\n    fun adds() {\n    }\n}\n",
        );
        assert_eq!(test_debugger(&root, &kt, "kotlin"), Some("java"));
        let d = debug("kotlin", &kt, 5);
        assert_eq!(d.adapter, "java");
        let script = gradle_debug_script().unwrap();
        assert!(
            std::fs::read_to_string(&script)
                .unwrap()
                .contains(JDWP_AGENT)
        );
        assert_eq!(
            d.args[d.args.len() - 6..],
            [
                "--tests".to_string(),
                "acme.CartTest.adds".into(),
                "--init-script".into(),
                gradle_test_script().unwrap().to_string_lossy().into_owned(),
                "--init-script".into(),
                script.to_string_lossy().into_owned()
            ]
        );
        let command: Vec<&str> = d.launch["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w.as_str().unwrap())
            .collect();
        assert_eq!(command[0], bin.join("gradle").to_string_lossy());
        assert_eq!(
            command[1..],
            d.args.iter().map(String::as_str).collect::<Vec<_>>()
        );

        write("mv/pom.xml", "<project/>\n");
        let java = write(
            "mv/src/test/java/acme/CartTest.java",
            "package acme;\nclass CartTest {\n  @Test\n  void adds() {}\n}\n",
        );
        let d = debug("java", &java, 4);
        // A port of Clew's, the debugger attaching to it: Surefire says
        // nothing of where its JVM listens.
        let port = d.launch["port"].as_u64().unwrap();
        assert_eq!(
            d.args.last().unwrap(),
            &format!(
                "-Dmaven.surefire.debug={}",
                JDWP_AGENT.replace("127.0.0.1:0", &format!("127.0.0.1:{port}"))
            )
        );

        write("sb/build.sbt", "");
        let scala = write(
            "sb/src/test/scala/acme/CartSuite.scala",
            "package acme\n\nclass CartSuite extends munit.FunSuite {\n  test(\"adds\") {\n  }\n}\n",
        );
        let d = debug("scala", &scala, 4);
        let port = d.launch["port"].as_u64().unwrap();
        let agent = JDWP_AGENT.replace("127.0.0.1:0", &format!("127.0.0.1:{port}"));
        assert_eq!(
            d.args[..2],
            [
                "set ThisBuild / Test / fork := true".to_string(),
                format!("set ThisBuild / Test / javaOptions += \"{agent}\""),
            ],
            "the tests' JVM forked, the agent on it alone"
        );
        assert!(d.args[2].starts_with("testOnly acme.CartSuite"));
        // A build that forks already is debugged the same way.
        write("fk/build.sbt", "Test / fork := true\n");
        let forked = write("fk/src/test/scala/acme/CartSuite.scala", "");
        assert_eq!(test_debugger(&root, &forked, "scala"), Some("java"));
        // Mill's tests in Mill's own JVM, under the agent; scala-cli's (a
        // file with no build) its test JVM's, before MUnit's words.
        write("ml/build.mill", "");
        write("ml/.mill-jvm-opts", "-Xmx2g");
        let mill = write(
            "ml/foo/test/src/CartSuite.scala",
            "class CartSuite extends munit.FunSuite {\n  test(\"adds\") {\n  }\n}\n",
        );
        assert_eq!(test_debugger(&root, &mill, "scala"), Some("java"));
        let d = debug("scala", &mill, 2);
        assert_eq!(
            d.args,
            ["--no-server", "foo.test.testLocal", "CartSuite.adds"],
            "Mill's tests in its own JVM, one by MUnit's name"
        );
        // That JVM — not the launcher's, which `JAVA_OPTS` reaches — starts
        // with the build's own options and then the agent's.
        let port = d.launch["port"].as_u64().unwrap();
        let opts = d.launch["env"]["MILL_JVM_OPTS_PATH"].as_str().unwrap();
        assert_eq!(
            std::fs::read_to_string(opts).unwrap(),
            format!(
                "-Xmx2g\n{}\n",
                JDWP_AGENT.replace("127.0.0.1:0", &format!("127.0.0.1:{port}"))
            )
        );
        assert!(d.launch["env"].get("JAVA_OPTS").is_none());
        assert_eq!(
            mill_module(&root.join("ml"), &mill).as_deref(),
            Some("foo.test")
        );
        assert_eq!(
            mill_module(
                &root.join("ml"),
                &root.join("ml/foo/src/main/scala/Main.scala")
            )
            .as_deref(),
            Some("foo")
        );
        assert_eq!(
            mill_module(&root.join("ml"), &root.join("ml/build.mill")),
            None
        );
        let mill_main = write(
            "ml/foo/src/Main.scala",
            "object Main:\n  def main(args: Array[String]): Unit = ()\n",
        );
        assert_eq!(main_debugger(&root, &mill_main, "scala"), Some("java"));
        let cli = write(
            "cli2/Cart.test.scala",
            "class CartSuite extends munit.FunSuite {\n  test(\"adds\") {\n  }\n}\n",
        );
        assert_eq!(test_debugger(&root, &cli, "scala"), Some("java"));
        let d = debug("scala", &cli, 2);
        let port = d.launch["port"].as_u64().unwrap();
        let agent = JDWP_AGENT.replace("127.0.0.1:0", &format!("127.0.0.1:{port}"));
        assert_eq!(
            d.args[2..],
            [
                "--test-only".to_string(),
                "CartSuite".into(),
                "--java-opt".into(),
                agent,
                "--".into(),
                "CartSuite.adds".into()
            ]
        );

        assert_eq!(test_debugger(&root, &go, "haskell"), None);

        // A Cabal suite's `main` under GHCi, in the suite's REPL, asked for
        // the test by hspec's `--match`.
        write(
            "hs/cart.cabal",
            "name: cart\n\nlibrary\n  hs-source-dirs: src\n\ntest-suite spec\n  type: exitcode-stdio-1.0\n  main-is: Spec.hs\n  hs-source-dirs: test\n",
        );
        write("hs/test/Spec.hs", "main = hspec CartSpec.spec\n");
        let hs = write(
            "hs/test/CartSpec.hs",
            "module CartSpec (spec) where\n\nspec = describe \"Cart\" $ do\n  it \"adds\" $\n    1 `shouldBe` 1\n",
        );
        assert_eq!(test_debugger(&root, &hs, "haskell"), Some("haskell"));
        let d = debug("haskell", &hs, 4);
        assert_eq!(d.adapter, "haskell");
        assert_eq!(d.program, root.join("hs/test/Spec.hs"));
        assert_eq!(d.args, ["--match", "adds"]);
        assert_eq!(d.launch["ghci"], "cabal repl test:spec");

        // C#'s by `dotnet test`, its host waiting for the debugger.
        write("cs/Shop.Tests.csproj", "<Project/>\n");
        let cs = write(
            "cs/CartTests.cs",
            "namespace Shop.Tests;\n\npublic class CartTests\n{\n    [Fact]\n    public void Adds()\n    {\n    }\n}\n",
        );
        assert_eq!(test_debugger(&root, &cs, "csharp"), Some("csharp"));
        let d = debug("csharp", &cs, 6);
        assert_eq!((d.adapter, &d.start), ("csharp", &DebugStart::DotnetTests));
        assert_eq!(
            d.launch["command"][0],
            bin.join("dotnet").to_string_lossy().as_ref()
        );
        assert_eq!(d.launch["command"][1], "test");

        // EUnit's under OTP's interpreter, compiled with TEST, by EUnit's
        // own call; a Common Test suite's, without.
        write("erl/rebar.config", "");
        let erl = write(
            "erl/test/cart_tests.erl",
            "-module(cart_tests).\nadd_test() ->\n    ok.\nadds_test_() ->\n    [].\n",
        );
        assert_eq!(test_debugger(&root, &erl, "erlang"), Some("erlang"));
        let d = debug("erlang", &erl, 2);
        assert_eq!((d.adapter, &d.cwd), ("erlang", &root.join("erl")));
        assert_eq!(
            d.launch["run"],
            "eunit:test({timeout, 86400, {test, 'cart_tests', 'add_test'}}, [verbose])"
        );
        assert_eq!(d.launch["defines"], serde_json::json!(["TEST"]));
        let d = debug("erlang", &erl, 4);
        assert_eq!(
            d.launch["run"],
            "eunit:test({timeout, 86400, {generator, 'cart_tests', 'adds_test_'}}, [verbose])"
        );
        // The whole file: each of its tests held, as a timeout around the
        // module is not its tests'.
        let run = debug("erlang", &erl, 1).launch["run"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            run.starts_with("eunit:test([{timeout, 86400, case ")
                && run.contains("{test, 'cart_tests', F}")
                && run.contains("{generator, 'cart_tests', F}")
                && run.contains("'cart_tests':module_info(exports)"),
            "{run}"
        );
        let suite = write("erl/test/cart_SUITE.erl", "-module(cart_SUITE).\n");
        assert_eq!(test_debugger(&root, &suite, "erlang"), None);

        // ExUnit's as `mix test file:line` runs them, in the test
        // environment: the test file compiled first, for its breakpoints.
        write("ex/mix.exs", "");
        let ex = write(
            "ex/test/cart_test.exs",
            "defmodule CartTest do\n  test \"adds\" do\n  end\nend\n",
        );
        assert_eq!(test_debugger(&root, &ex, "elixir"), Some("elixir"));
        let d = debug("elixir", &ex, 2);
        assert_eq!(d.adapter, "elixir");
        assert_eq!(d.launch["mixEnv"], "test");
        assert_eq!(
            d.launch["projectDir"],
            root.join("ex").to_string_lossy().as_ref()
        );
        let load = d.launch["load"].as_str().unwrap();
        assert!(load.contains("Code.compile_file(file)"), "{load}");
        assert!(load.contains(&ex.to_string_lossy().into_owned()), "{load}");
        let run = d.launch["run"].as_str().unwrap();
        assert!(
            run.contains("ExUnit.Filters.parse_path(\"test/cart_test.exs:2\")"),
            "{run}"
        );
        assert!(run.ends_with("if failures > 0, do: 1, else: 0"), "{run}");
        // Held as long as it is read: ExUnit fails a test after a minute.
        assert!(
            run.starts_with("ExUnit.configure(timeout: :infinity)\n"),
            "{run}"
        );
    }

    /// Each program debugged runs as Run runs it, under its language's
    /// debugger: Gradle's `run` with Clew's init script, a Kotlin script's
    /// kotlinc with the agent in `JAVA_OPTS`, sbt with the agent on its JVM,
    /// scala-cli with it as a Java option, an Elixir script or application
    /// in its Mix project; none for an sbt build that forks its run.
    #[test]
    fn each_debugged_program_runs_under_its_languages_debugger() {
        let _data = crate::testutil::DataDir::new("langtests-main-data");
        let tmp = crate::testutil::TempDir::new("langtests-main");
        let root = tmp.path().to_path_buf();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        };
        let bin = root.join("bin");
        for name in [
            "gradle",
            "kotlinc",
            "sbt",
            "scala-cli",
            "mill",
            "mix",
            "elixir",
            "dotnet",
            "zig",
        ] {
            let path = write(&format!("bin/{name}"), "#!/bin/sh\n");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let search = bin.as_os_str();
        let debug = |lang: &str, file: &Path| {
            assert!(main_debugger(&root, file, lang).is_some(), "{lang}");
            debug_main(&root, file, lang, search, &mut |_| {}, &Default::default())
                .unwrap_or_else(|e| panic!("{lang}: {e}"))
        };
        let command = |d: &DebugTest| -> Vec<String> {
            d.launch["command"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w.as_str().unwrap().to_string())
                .collect()
        };

        write("gr/build.gradle.kts", "");
        let kt = write("gr/src/main/kotlin/acme/Main.kt", "fun main() {}\n");
        let d = debug("kotlin", &kt);
        assert_eq!(d.adapter, "java");
        let words = command(&d);
        assert_eq!(words[0], bin.join("gradle").to_string_lossy());
        assert!(words.contains(&"run".to_string()) && words.contains(&"--init-script".to_string()));

        let kts = write("scripts/hello.main.kts", "println(1)\n");
        let d = debug("kotlin", &kts);
        assert_eq!(
            command(&d)[1..],
            ["-script".to_string(), kts.to_string_lossy().into_owned()]
        );
        assert!(
            d.launch["env"]["JAVA_OPTS"]
                .as_str()
                .unwrap()
                .ends_with(JDWP_AGENT)
        );

        write("sb/build.sbt", "");
        let sc = write("sb/src/main/scala/Main.scala", "object Main\n");
        let agent = |d: &DebugTest| {
            let port = d.launch["port"].as_u64().unwrap();
            JDWP_AGENT.replace("127.0.0.1:0", &format!("127.0.0.1:{port}"))
        };
        let d = debug("scala", &sc);
        assert_eq!(
            command(&d)[1..],
            [
                "set ThisBuild / run / fork := true".to_string(),
                format!("set ThisBuild / run / javaOptions += \"{}\"", agent(&d)),
                "run".into()
            ]
        );
        assert_eq!(
            scala_main_class("package shop\n\nobject Main:\n  def main(args: Array[String]): Unit =\n    println(1)\n").as_deref(),
            Some("shop.Main")
        );
        assert_eq!(
            scala_main_class("package a\npackage b\n\n@main def hello(): Unit = ()\n").as_deref(),
            Some("a.b.hello")
        );
        assert_eq!(
            scala_main_class("object Shop extends App {\n  println(1)\n}\n").as_deref(),
            Some("Shop")
        );
        let alone = write("cli/hello.scala", "@main def hi() = ()\n");
        let d = debug("scala", &alone);
        let words = command(&d);
        assert_eq!(
            words[words.len() - 2..],
            ["--java-opt".to_string(), agent(&d)]
        );
        write("fk/build.sbt", "run / fork := true\n");
        let forked = write("fk/src/main/scala/Main.scala", "object Main\n");
        assert_eq!(main_debugger(&root, &forked, "scala"), Some("java"));
        // Mill's program in Mill's own JVM (`runMainLocal`, no server), the
        // agent in that JVM's options file.
        write("ml/build.mill", "");
        let ml = write(
            "ml/foo/src/Main.scala",
            "package shop\n\nobject Main:\n  def main(args: Array[String]): Unit = ()\n",
        );
        let d = debug("scala", &ml);
        assert_eq!(
            command(&d)[1..],
            ["--no-server", "foo.runMainLocal", "shop.Main"]
        );
        let opts = d.launch["env"]["MILL_JVM_OPTS_PATH"].as_str().unwrap();
        assert_eq!(
            std::fs::read_to_string(opts).unwrap(),
            format!("{}\n", agent(&d))
        );

        write(
            "ex/mix.exs",
            "def application, do: [mod: {Cart.Application, []}]\n",
        );
        let script = write("ex/scripts/seed.exs", "IO.puts(1)\n");
        let d = debug("elixir", &script);
        assert_eq!(
            (d.adapter, &d.launch["mixEnv"]),
            ("elixir", &serde_json::json!("dev"))
        );
        let run = d.launch["run"].as_str().unwrap();
        assert!(run.starts_with("Mix.Task.rerun(\"app.start\")"), "{run}");
        assert!(run.contains("Code.require_file("), "{run}");
        let app = write("ex/lib/cart.ex", "defmodule Cart do\nend\n");
        let run = debug("elixir", &app).launch["run"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            run.contains("Process.sleep(:infinity)"),
            "an application runs on: {run}"
        );
        // A .NET program, not its library nor its tests; a Zig file alone.
        write(
            "cs/App/App.csproj",
            "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><OutputType>Exe</OutputType></PropertyGroup></Project>\n",
        );
        let program = write("cs/App/Program.cs", "Console.WriteLine(1);\n");
        assert_eq!(main_debugger(&root, &program, "csharp"), Some("csharp"));
        let run = main_command(&root, &program, "csharp", search).unwrap();
        assert_eq!(run.args[..2], ["run".to_string(), "--project".into()]);
        write(
            "cs/Tests/Tests.csproj",
            "<Project><ItemGroup><PackageReference Include=\"Microsoft.NET.Test.Sdk\" /></ItemGroup></Project>\n",
        );
        let tests = write("cs/Tests/CartTests.cs", "public class CartTests {}\n");
        assert_eq!(main_debugger(&root, &tests, "csharp"), None);
        assert!(main_command(&root, &tests, "csharp", search).is_err());
        let zig = write("z/hello.zig", "pub fn main() void {}\n");
        assert_eq!(main_debugger(&root, &zig, "zig"), Some("native"));
        let d = debug("zig", &zig);
        assert_eq!((d.adapter, &d.program), ("native", &zig));
        assert_eq!(
            main_command(&root, &zig, "zig", search).unwrap().args[0],
            "run"
        );
        let only_tests = write("z/cart.zig", "test \"adds\" {}\n");
        assert_eq!(main_debugger(&root, &only_tests, "zig"), None);
        assert!(main_command(&root, &only_tests, "zig", search).is_err());
        write("zb/build.zig", "");
        let built = write("zb/src/main.zig", "pub fn main() void {}\n");
        assert_eq!(main_debugger(&root, &built, "zig"), Some("native"));
        let d = debug("zig", &built);
        assert_eq!((d.adapter, &d.program), ("native", &built));
        assert_eq!(d.cwd, root.join("zb"), "where `zig build run` runs it");
        assert_eq!(
            main_command(&root, &built, "zig", search).unwrap().args,
            ["build", "run"]
        );
        // C and C++: a CMake target's source, a file alone with a main;
        // a make project's is a launch file's to name.
        write(
            "cm/CMakeLists.txt",
            "project(x CXX)\nadd_executable(greeter\n  src/main.cpp src/util.cpp)\n",
        );
        let main_cpp = write("cm/src/main.cpp", "int main() { return 0; }\n");
        let util = write("cm/src/util.cpp", "int util() { return 1; }\n");
        assert_eq!(
            cmake_target(&root, &main_cpp).map(|t| t.0).as_deref(),
            Some("greeter")
        );
        assert_eq!(
            cmake_target(&root, &util).map(|t| t.0).as_deref(),
            Some("greeter")
        );
        assert_eq!(main_debugger(&root, &util, "cpp"), Some("native"));
        let alone = write(
            "one/hello.c",
            "#include <stdio.h>\nint main(void) { return 0; }\n",
        );
        assert_eq!(main_debugger(&root, &alone, "c"), Some("native"));
        write("mk/Makefile", "all:\n");
        let made = write("mk/app.c", "int main(void) { return 0; }\n");
        assert_eq!(main_debugger(&root, &made, "c"), None);
        let no_main = write("one/lib.c", "int add(int a, int b) { return a + b; }\n");
        assert_eq!(main_debugger(&root, &no_main, "c"), None);
        let domain = write("one/dom.c", "int domain(int x) {\n    return x;\n}\n");
        assert_eq!(main_debugger(&root, &domain, "c"), None);
        let lone = write("lone/hi.exs", "IO.puts(1)\n");
        let d = debug("elixir", &lone);
        assert!(d.launch.get("projectDir").is_none());
        assert!(
            d.launch["run"]
                .as_str()
                .unwrap()
                .starts_with("Code.require_file(")
        );
    }
}
