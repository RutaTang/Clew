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
        "haskell" => called_tests(
            &lines,
            &["it", "prop", "specify", "testCase", "testProperty"],
            &["describe", "context", "testGroup"],
        ),
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
    out
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
/// and doctest's `TEST_CASE("name")`.
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
                (true, _) => {}
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
            if let Some(dir) = above(root, file, &["build.sbt"]) {
                // ScalaTest picks a test by its name's text (`-z`); MUnit
                // and JUnit run their class's.
                let scalatest = source.contains("org.scalatest");
                let filter = match (runnable.kind, &class) {
                    (RunnableKind::Test, Some((class, _))) if scalatest => {
                        format!("testOnly {class} -- -z \"{}\"", runnable.name)
                    }
                    (_, Some((class, _))) => format!("testOnly {class}"),
                    (_, None) => "test".into(),
                };
                return Ok(command(tool("sbt")?, vec![filter], &dir));
            }
            if let Some(dir) = above(root, file, &["build.mill", "build.sc"]) {
                let mut args = vec!["__.test.testOnly".to_string()];
                args.extend(class.map(|(c, _)| c));
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
                let mut args = vec![
                    "-p".to_string(),
                    dir.to_string_lossy().into_owned(),
                    "test".into(),
                ];
                if let Some((class, method)) = class {
                    args.push("--tests".into());
                    args.push(match method {
                        Some(method) => format!("{class}.{method}"),
                        None => class,
                    });
                }
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
                }
                return Ok(command(maven, args, &dir));
            }
            if lang == "scala" {
                // A scala-cli project: its folder, or the file alone.
                let dir = file.parent().unwrap_or(root);
                let mut args = vec!["test".to_string(), dir.to_string_lossy().into_owned()];
                if let Some((class, _)) = class {
                    args.extend(["--test-only".into(), class]);
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
        _ => match runnable.scopes.first() {
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

        let hs =
            "spec :: Spec\nspec = describe \"Cart\" $ do\n  it \"adds\" $ do\n    1 `shouldBe` 1\n";
        let got: Vec<String> = tests("haskell", "test/CartSpec.hs", hs)
            .iter()
            .map(|r| r.name.clone())
            .collect();
        assert_eq!(got, ["", "adds"]);

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
        assert_eq!(got, ["", "adds"]);
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
                "test".into(),
                "--tests".into(),
                "com.acme.CartTest.adds".into()
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
        assert_eq!(c.args, ["testOnly acme.CartSuite"], "MUnit's: its class");
        let scalatest = write(
            "sc/src/test/scala/acme/CartSpec.scala",
            "package acme\n\nimport org.scalatest.funsuite.AnyFunSuite\n\nclass CartSpec extends AnyFunSuite {\n  test(\"adds\") {\n  }\n}\n",
        );
        let c = run("scala", &scalatest, 6);
        assert_eq!(c.args, ["testOnly acme.CartSpec -- -z \"adds\""]);
    }
}
