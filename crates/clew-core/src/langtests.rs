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
        "java" | "kotlin" | "scala" => jvm_debug_build(root, file).map(|_| "java"),
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
}

/// The build that runs `file`'s tests where the JVM debugger reaches them,
/// found as [`test_command`] finds it: Gradle's and Maven's (each test JVM
/// started under the agent), and sbt's when it runs them in its own JVM —
/// with `fork := true` they run in one it starts without it.
fn jvm_debug_build(root: &Path, file: &Path) -> Option<JvmBuild> {
    if let Some(dir) = above(root, file, &["build.sbt"]) {
        return (!sbt_forks(&dir)).then_some(JvmBuild::Sbt);
    }
    if above(root, file, &["build.mill", "build.sc"]).is_some() {
        return None;
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

/// Whether the sbt build in `dir` forks its tests or programs (`fork :=
/// true`): they run in a JVM it starts, where the agent on its own is not.
fn sbt_forks(dir: &Path) -> bool {
    crate::statefile::read_capped(&dir.join("build.sbt"), 1 << 20).is_some_and(|text| {
        text.lines()
            .any(|l| l.contains("fork") && l.contains(":= true"))
    })
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
        "scala" => match above(root, file, &["build.sbt"]) {
            Some(dir) => (!sbt_forks(&dir)).then_some("java"),
            None => Some("java"),
        },
        "elixir" => Some("elixir"),
        // A .NET program, built and run under netcoredbg; a Zig file on
        // its own, built for the session under lldb (a build.zig's
        // program is the build's to make: it runs).
        "csharp" => dotnet_project(root, file)
            .filter(|(_, program)| *program)
            .map(|_| "csharp"),
        "zig" => {
            (above(root, file, &["build.zig"]).is_none() && has_zig_main(file)).then_some("native")
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
) -> Result<DebugTest, String> {
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
            let sbt = run
                .program
                .file_stem()
                .is_some_and(|n| n.to_string_lossy() == "sbt");
            match sbt {
                true => args.insert(0, format!("-J{agent}")),
                false => args.extend(["--java-opt".to_string(), agent]),
            }
            let mut debug = jvm(run.program, args, run.cwd);
            debug.launch.insert("port".into(), port.into());
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
            let built = crate::lsp::store::run_bounded(build, std::time::Duration::from_secs(900))?;
            let said = String::from_utf8_lossy(&built.stdout).into_owned();
            if !built.status.success() {
                let tail: Vec<&str> = said.lines().rev().take(30).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                return Err(format!(
                    "dotnet could not build {}:\n{}",
                    project
                        .file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_default(),
                    tail.join("\n")
                ));
            }
            let assembly = said
                .lines()
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
        "zig" => {
            let dir = file.parent().unwrap_or(root).to_path_buf();
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
            let call = match (whole_file, runnable.name.ends_with("_test_")) {
                // Code, not words: put together, not formatted.
                (true, _) => ["eunit:test(", &erlang_atom(&module), ", [verbose])"].concat(),
                (false, true) => format!(
                    "eunit:test({{generator, {}, {}}}, [verbose])",
                    erlang_atom(&module),
                    erlang_atom(&runnable.name)
                ),
                (false, false) => format!(
                    "eunit:test({{test, {}, {}}}, [verbose])",
                    erlang_atom(&module),
                    erlang_atom(&runnable.name)
                ),
            };
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
            let run = format!(
                "{filter}%{{failures: failures}} = ExUnit.run()\nif failures > 0, do: 1, else: 0"
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
            let Some(build) = jvm_debug_build(root, file) else {
                return Err("This build runs its tests where the debugger can't reach them, so they run without it.".to_string());
            };
            let mut args = run.args;
            let mut agent_port: Option<u16> = None;
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
                // sbt's own JVM, on a port of Clew's too: what the launcher
                // prints is not counted on.
                JvmBuild::Sbt => {
                    let port = free_port()?;
                    let agent = JDWP_AGENT.replace("127.0.0.1:0", &format!("127.0.0.1:{port}"));
                    args.insert(0, format!("-J{agent}"));
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
            Ok(debug)
        }
        _ => Err(format!("{rel} has no tests to run", rel = lang)),
    }
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

/// Gradle's init script that starts each test JVM, and an application's
/// `run`, under the debugging agent — one JVM at a time — written once to
/// Clew's data directory.
fn gradle_debug_script() -> Result<PathBuf, String> {
    const SCRIPT: &str = "// Written by Clew to debug a test or a program: its JVM starts under\n// the debugging agent, held until the debugger attaches.\ngradle.allprojects { project ->\n    project.tasks.withType(Test).configureEach { task ->\n        task.jvmArgs '-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=127.0.0.1:0'\n        task.maxParallelForks = 1\n    }\n    project.tasks.withType(JavaExec).configureEach { task ->\n        if (task.name == 'run') {\n            task.jvmArgs '-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=127.0.0.1:0'\n        }\n    }\n}\n";
    let dir = crate::lsp::store::data_root()
        .unwrap_or_else(std::env::temp_dir)
        .join("debug-adapters")
        .join("gradle");
    let path = dir.join("clew-debug.gradle");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(SCRIPT) {
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(&path, SCRIPT))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    Ok(path)
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
                return Ok(command(tool("sbt")?, vec!["run".into()], &dir));
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
            d.args[d.args.len() - 4..],
            [
                "--tests".to_string(),
                "acme.CartTest.adds".into(),
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
        assert_eq!(
            d.args[0],
            format!(
                "-J{}",
                JDWP_AGENT.replace("127.0.0.1:0", &format!("127.0.0.1:{port}"))
            )
        );
        // Forked, its tests run in a JVM the agent is not in.
        write("fk/build.sbt", "Test / fork := true\n");
        let forked = write("fk/src/test/scala/acme/CartSuite.scala", "");
        assert_eq!(test_debugger(&root, &forked, "scala"), None);

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
            "eunit:test({test, 'cart_tests', 'add_test'}, [verbose])"
        );
        assert_eq!(d.launch["defines"], serde_json::json!(["TEST"]));
        let d = debug("erlang", &erl, 4);
        assert_eq!(
            d.launch["run"],
            "eunit:test({generator, 'cart_tests', 'adds_test_'}, [verbose])"
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
            debug_main(&root, file, lang, search).unwrap_or_else(|e| panic!("{lang}: {e}"))
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
        assert_eq!(command(&d)[1..], [format!("-J{}", agent(&d)), "run".into()]);
        let alone = write("cli/hello.scala", "@main def hi() = ()\n");
        let d = debug("scala", &alone);
        let words = command(&d);
        assert_eq!(
            words[words.len() - 2..],
            ["--java-opt".to_string(), agent(&d)]
        );
        write("fk/build.sbt", "run / fork := true\n");
        let forked = write("fk/src/main/scala/Main.scala", "object Main\n");
        assert_eq!(main_debugger(&root, &forked, "scala"), None);

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
        assert_eq!(main_debugger(&root, &built, "zig"), None);
        assert_eq!(
            main_command(&root, &built, "zig", search).unwrap().args,
            ["build", "run"]
        );
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
