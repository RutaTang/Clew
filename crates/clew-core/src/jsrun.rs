//! JavaScript and TypeScript run as their project runs them: a test, a suite
//! or a test file by the project's own test runner (Vitest, Jest, Mocha,
//! Playwright, Bun's, Node's or Deno's), a program by its runtime (Deno,
//! Bun, or Node — TypeScript with its types stripped, or by the project's
//! `tsx`), and a package's scripts by its package manager. Worked out on the
//! project's host from the project's own files, never from a command line
//! the asker names (see [`crate::tasks`]).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::runnables::{RunnableKind, runnable_at, runnables};

/// A test runner a JavaScript or TypeScript project runs its tests with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    Vitest,
    Jest,
    Mocha,
    Playwright,
    Bun,
    Node,
    Deno,
}

impl Runner {
    /// What it is called.
    pub fn name(self) -> &'static str {
        match self {
            Runner::Vitest => "Vitest",
            Runner::Jest => "Jest",
            Runner::Mocha => "Mocha",
            Runner::Playwright => "Playwright",
            Runner::Bun => "Bun",
            Runner::Node => "Node.js",
            Runner::Deno => "Deno",
        }
    }
}

/// A program to run: what, with what arguments, in which folder, with which
/// variables set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

/// A `package.json` is a few kilobytes; the cap stops one the repository
/// planted from being read without limit.
const MAX_MANIFEST_BYTES: u64 = 1 << 20;

/// The folders from `file`'s up to `root`, nearest first, that hold a
/// `package.json`: its package's, then those it sits in (a monorepo's
/// root, where its tools are often installed).
fn package_dirs(root: &Path, file: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dir = file.parent();
    while let Some(d) = dir {
        if !d.starts_with(root) {
            break;
        }
        if d.join("package.json").is_file() {
            out.push(d.to_path_buf());
        }
        if d == root {
            break;
        }
        dir = d.parent();
    }
    out
}

/// The folder of the package `file` belongs to: the nearest at or above it,
/// within `root`, with a `package.json`; the project's root without one.
pub fn package_dir(root: &Path, file: &Path) -> PathBuf {
    package_dirs(root, file)
        .into_iter()
        .next()
        .unwrap_or_else(|| root.to_path_buf())
}

/// The `package.json` of `dir`, read.
fn manifest(dir: &Path) -> Option<Value> {
    let text = crate::statefile::read_capped(&dir.join("package.json"), MAX_MANIFEST_BYTES)?;
    serde_json::from_str(&text).ok()
}

/// Whether the package `manifest` describes depends on `name`.
fn depends_on(manifest: &Value, name: &str) -> bool {
    [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ]
    .iter()
    .any(|k| manifest.get(k).and_then(|d| d.get(name)).is_some())
}

/// The package's `test` script, if it has one.
fn test_script(manifest: &Value) -> Option<&str> {
    manifest.get("scripts")?.get("test")?.as_str()
}

/// Whether `dir` is a Deno project's (a `deno.json` or `deno.jsonc`).
fn is_deno(dir: &Path) -> bool {
    ["deno.json", "deno.jsonc"]
        .iter()
        .any(|n| dir.join(n).is_file())
}

/// Whether `dir` is a Bun project's: Bun's lockfile, or a `packageManager`
/// that names Bun.
fn is_bun(dir: &Path, manifest: Option<&Value>) -> bool {
    ["bun.lock", "bun.lockb"]
        .iter()
        .any(|n| dir.join(n).is_file())
        || manifest
            .and_then(|m| m.get("packageManager"))
            .and_then(Value::as_str)
            .is_some_and(|pm| pm.starts_with("bun@"))
}

/// The test runner `file`'s tests are run with: the one it imports its test
/// functions from (`vitest`, `bun:test`, `node:test`, `@jest/globals`,
/// `@playwright/test`, Deno's); else the one its package — or a package it
/// sits in — depends on or runs its `test` script with; else Bun's for a
/// Bun project, Deno's for a Deno one. `None` when nothing says.
pub fn test_runner(root: &Path, file: &Path, source: &str) -> Option<Runner> {
    let imports =
        |m: &str| source.contains(&format!("'{m}'")) || source.contains(&format!("\"{m}\""));
    if imports("vitest") {
        return Some(Runner::Vitest);
    }
    if imports("bun:test") {
        return Some(Runner::Bun);
    }
    if imports("node:test") {
        return Some(Runner::Node);
    }
    if imports("@playwright/test") {
        return Some(Runner::Playwright);
    }
    if imports("@jest/globals") {
        return Some(Runner::Jest);
    }
    if source.contains("Deno.test(") || source.contains("jsr:@std/testing") {
        return Some(Runner::Deno);
    }
    let dirs = package_dirs(root, file);
    if dirs.is_empty() && is_deno(root) {
        return Some(Runner::Deno);
    }
    for dir in &dirs {
        if is_deno(dir) {
            return Some(Runner::Deno);
        }
        let manifest = manifest(dir);
        if let Some(m) = &manifest {
            for (package, runner) in [
                ("vitest", Runner::Vitest),
                ("jest", Runner::Jest),
                ("react-scripts", Runner::Jest),
                ("mocha", Runner::Mocha),
            ] {
                if depends_on(m, package) {
                    return Some(runner);
                }
            }
            if let Some(script) = test_script(m) {
                for (word, runner) in [
                    ("vitest", Runner::Vitest),
                    ("jest", Runner::Jest),
                    ("mocha", Runner::Mocha),
                    ("bun test", Runner::Bun),
                    ("node --test", Runner::Node),
                    ("deno test", Runner::Deno),
                    ("playwright test", Runner::Playwright),
                ] {
                    if script.contains(word) {
                        return Some(runner);
                    }
                }
            }
            if depends_on(m, "@playwright/test") {
                return Some(Runner::Playwright);
            }
        }
        if is_bun(dir, manifest.as_ref()) {
            return Some(Runner::Bun);
        }
    }
    None
}

/// The program `name` the project installed for itself — its
/// `node_modules/.bin/<name>` — in `file`'s package or one it sits in,
/// nearest first.
fn local_bin(root: &Path, file: &Path, name: &str) -> Option<PathBuf> {
    let names: &[String] = &if cfg!(windows) {
        vec![format!("{name}.cmd"), name.to_string()]
    } else {
        vec![name.to_string()]
    };
    let mut dirs = package_dirs(root, file);
    if !dirs.iter().any(|d| d == root) {
        dirs.push(root.to_path_buf());
    }
    dirs.iter()
        .flat_map(|d| {
            names
                .iter()
                .map(move |n| d.join("node_modules").join(".bin").join(n))
        })
        .find(|p| p.is_file())
}

/// A runtime of this host — `node`, `bun`, `deno` — looked for on `search`.
fn runtime(search: &OsStr, name: &str) -> Result<PathBuf, String> {
    crate::lsp::store::find_in_unresolved(search, name).ok_or_else(|| match name {
        "node" => "Node.js wasn't found here. Install Node.js, then run again.".to_string(),
        "bun" => "Bun wasn't found here. Install Bun, then run again.".to_string(),
        "deno" => "Deno wasn't found here. Install Deno, then run again.".to_string(),
        other => format!("{other} wasn't found here."),
    })
}

/// One title as a regular expression: its text matched as written, but a
/// `test.each` placeholder (`%s`, `%i`, `%#`, `$name`) and a template's
/// `${…}` match anything, as the runner fills them in.
fn title_pattern(title: &str) -> String {
    let title = title.trim();
    let title = title
        .strip_prefix('`')
        .and_then(|t| t.strip_suffix('`'))
        .unwrap_or(title);
    let mut out = String::new();
    let mut chars = title.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '%' if chars.peek() == Some(&'%') => {
                chars.next();
                out.push('%');
            }
            '%' if chars.peek().is_some_and(|n| "sdifjopc#".contains(*n)) => {
                chars.next();
                out.push_str(".*");
            }
            '$' if chars.peek() == Some(&'{') => {
                let mut depth = 0;
                for n in chars.by_ref() {
                    match n {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                out.push_str(".*");
            }
            '$' if chars.peek().is_some_and(|n| n.is_alphabetic() || *n == '_') => {
                while chars
                    .peek()
                    .is_some_and(|n| n.is_alphanumeric() || matches!(n, '_' | '.'))
                {
                    chars.next();
                }
                out.push_str(".*");
            }
            c if "\\.^$|?*+()[]{}/".contains(c) => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// A test's full name as a regular expression — its suites' titles, then its
/// own, joined with spaces, as Vitest, Jest, Bun, Mocha and Node name a test
/// — ending there for a test (`whole`), or followed by the names of the
/// tests under a suite. Not anchored at its start: a runner may put the
/// file's name first.
fn name_pattern(names: &[String], whole: bool) -> String {
    let parts: Vec<String> = names.iter().map(|n| title_pattern(n)).collect();
    let end = match whole {
        true => "$",
        false => "( |$)",
    };
    format!("(^| ){}{end}", parts.join(" "))
}

/// `file` as the runner in `cwd` is given it: relative, with `/`.
fn rel_arg(cwd: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(cwd).unwrap_or(file);
    let s = rel.to_string_lossy().replace('\\', "/");
    match s.starts_with('.') || s.starts_with('/') {
        true => s,
        false => format!("./{s}"),
    }
}

/// What runs the test, the suite or the file's tests at 1-based `line` of
/// `file` (its text `source`, in language `lang`), and what the run is
/// called: the test's title, the suite's, or the file's name. `Err` says
/// why it cannot run — no test there, no runner the project names, the
/// runner not installed.
pub fn test_command(
    root: &Path,
    file: &Path,
    lang: &str,
    source: &str,
    line: usize,
    search: &OsStr,
) -> Result<(JsCommand, String), String> {
    let found = runnables(lang, source);
    let test = runnable_at(&found, line)
        .filter(|r| matches!(r.kind, RunnableKind::Test | RunnableKind::Tests))
        .ok_or_else(|| "There is no test here.".to_string())?;
    let runner = test_runner(root, file, source).ok_or_else(|| {
        "No test runner was found for this file: its package depends on none (Vitest, Jest, \
         Mocha, Playwright), and it imports none (node:test, bun:test)."
            .to_string()
    })?;
    let mut names = test.scopes.clone();
    if !test.name.is_empty() {
        names.push(test.name.clone());
    }
    let one = test.kind == RunnableKind::Test;
    let called = match names.last() {
        Some(name) => name.clone(),
        None => file
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
    };
    let cwd = package_dir(root, file);
    let rel = rel_arg(&cwd, file);
    let pattern = (!names.is_empty()).then(|| name_pattern(&names, one));
    let installed = |bin: &str| -> Result<PathBuf, String> {
        local_bin(root, file, bin).ok_or_else(|| {
            format!(
                "{} is not installed in this project: install its dependencies (npm install), \
                 then run again.",
                runner.name()
            )
        })
    };
    let mut args: Vec<String> = Vec::new();
    let program = match runner {
        Runner::Vitest => {
            args.extend(["run".into(), rel]);
            if let Some(p) = pattern {
                args.extend(["-t".into(), p]);
            }
            installed("vitest")?
        }
        Runner::Jest => {
            let (program, first): (PathBuf, &[&str]) = match local_bin(root, file, "jest") {
                Some(jest) => (jest, &[]),
                // Create React App runs Jest through its own scripts, with its
                // configuration.
                None => (installed("react-scripts")?, &["test", "--watchAll=false"]),
            };
            args.extend(first.iter().map(|a| a.to_string()));
            args.extend(["--runTestsByPath".into(), rel]);
            if let Some(p) = pattern {
                args.extend(["-t".into(), p]);
            }
            program
        }
        Runner::Mocha => {
            args.push(rel);
            if let Some(p) = pattern {
                args.extend(["-g".into(), p]);
            }
            installed("mocha")?
        }
        Runner::Playwright => {
            args.push("test".into());
            args.push(match test.name.is_empty() {
                true => rel,
                false => format!("{rel}:{}", test.line),
            });
            installed("playwright")?
        }
        Runner::Bun => {
            args.extend(["test".into(), rel]);
            if let Some(p) = pattern {
                args.extend(["-t".into(), p]);
            }
            runtime(search, "bun")?
        }
        Runner::Node => {
            // As a person reads it (`✔ adds`), not the TAP a pipe gets.
            args.extend(["--test".into(), "--test-reporter=spec".into()]);
            if is_typescript(file) {
                args.push("--experimental-strip-types".into());
            }
            if let Some(p) = pattern {
                args.push(format!("--test-name-pattern={p}"));
            }
            args.push(rel);
            runtime(search, "node")?
        }
        Runner::Deno => {
            args.extend(["test".into(), "--allow-all".into()]);
            if let Some(name) = names.last() {
                args.extend(["--filter".into(), name.trim_matches('`').to_string()]);
            }
            args.push(rel);
            runtime(search, "deno")?
        }
    };
    Ok((
        JsCommand {
            program,
            args,
            cwd,
            env: Vec::new(),
        },
        called,
    ))
}

/// Whether `file` is TypeScript (`.ts`, `.mts`, `.cts`).
fn is_typescript(file: &Path) -> bool {
    file.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|e| matches!(e, "ts" | "mts" | "cts"))
}

/// What runs `file` as a program: Deno, in a Deno project; Bun, in a Bun
/// project (TypeScript and JSX alike); else Node — TypeScript with its
/// types stripped (Node 22.6 and later), or by the project's own `tsx`,
/// which JSX needs. `Err` says why it cannot run.
pub fn main_command(root: &Path, file: &Path, search: &OsStr) -> Result<JsCommand, String> {
    let dirs = package_dirs(root, file);
    let cwd = dirs.first().cloned().unwrap_or_else(|| root.to_path_buf());
    let rel = rel_arg(&cwd, file);
    let ext = file
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let command = |program: PathBuf, args: Vec<String>| JsCommand {
        program,
        args,
        cwd: cwd.clone(),
        env: Vec::new(),
    };
    let deno = dirs.iter().any(|d| is_deno(d)) || (dirs.is_empty() && is_deno(root));
    if deno {
        return Ok(command(
            runtime(search, "deno")?,
            vec!["run".into(), "--allow-all".into(), rel],
        ));
    }
    let bun = dirs.iter().any(|d| is_bun(d, manifest(d).as_ref())) || is_bun(root, None);
    if bun {
        return Ok(command(runtime(search, "bun")?, vec!["run".into(), rel]));
    }
    let jsx = matches!(ext.as_str(), "jsx" | "tsx");
    if let Some(tsx) = local_bin(root, file, "tsx").filter(|_| jsx || is_typescript(file)) {
        return Ok(command(tsx, vec![rel]));
    }
    if jsx {
        return Err(
            "Node.js cannot run JSX by itself: add tsx to the project (npm install --save-dev \
             tsx), then run again."
                .to_string(),
        );
    }
    let mut args = Vec::new();
    if is_typescript(file) {
        args.push("--experimental-strip-types".to_string());
    }
    args.push(rel);
    Ok(command(runtime(search, "node")?, args))
}

/// A package manager a project installs and runs its scripts with.
pub fn package_manager(root: &Path, dir: &Path) -> &'static str {
    let mut dirs: Vec<PathBuf> = package_dirs(root, &dir.join("package.json"));
    if !dirs.iter().any(|d| d == root) {
        dirs.push(root.to_path_buf());
    }
    for d in &dirs {
        let named = manifest(d)
            .and_then(|m| m.get("packageManager")?.as_str().map(str::to_string))
            .and_then(|pm| {
                ["pnpm", "yarn", "bun", "npm"]
                    .into_iter()
                    .find(|n| pm.starts_with(&format!("{n}@")))
            });
        if let Some(pm) = named {
            return pm;
        }
        for (lock, pm) in [
            ("pnpm-lock.yaml", "pnpm"),
            ("yarn.lock", "yarn"),
            ("bun.lock", "bun"),
            ("bun.lockb", "bun"),
            ("package-lock.json", "npm"),
            ("npm-shrinkwrap.json", "npm"),
        ] {
            if d.join(lock).is_file() {
                return pm;
            }
        }
    }
    "npm"
}

/// The scripts of the package in `dir`, as its `package.json` names them:
/// each script's name and command, in the order written.
pub fn scripts(dir: &Path) -> Vec<(String, String)> {
    let Some(text) = crate::statefile::read_capped(&dir.join("package.json"), MAX_MANIFEST_BYTES)
    else {
        return Vec::new();
    };
    let Some(m) = serde_json::from_str::<Value>(&text).ok() else {
        return Vec::new();
    };
    let Some(scripts) = m.get("scripts").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = scripts
        .iter()
        .filter_map(|(name, cmd)| Some((name.clone(), cmd.as_str()?.to_string())))
        .collect();
    // In the order the file writes them (a JSON object read here keeps its
    // keys sorted): each by where its name stands after `"scripts"`.
    let from = text.find("\"scripts\"").unwrap_or(0);
    let at = |name: &str| {
        let quoted = serde_json::to_string(name).unwrap_or_default();
        text[from..].find(&quoted).unwrap_or(usize::MAX)
    };
    out.sort_by_key(|(name, _)| at(name));
    out
}

/// What runs the script `script` of the package in `dir`: its package
/// manager's `run`. `Err` when the package has no such script, or the
/// package manager is not here.
pub fn script_command(
    root: &Path,
    dir: &Path,
    script: &str,
    search: &OsStr,
) -> Result<JsCommand, String> {
    if !scripts(dir).iter().any(|(name, _)| name == script) {
        return Err(format!("The package has no script named {script}."));
    }
    let pm = package_manager(root, dir);
    let program = crate::lsp::store::find_in_unresolved(search, pm)
        .or_else(|| {
            (pm != "npm")
                .then(|| crate::lsp::store::find_in_unresolved(search, "npm"))
                .flatten()
        })
        .ok_or_else(|| format!("{pm} wasn't found here. Install Node.js, then run again."))?;
    Ok(JsCommand {
        program,
        args: vec!["run".into(), script.to_string()],
        cwd: dir.to_path_buf(),
        env: Vec::new(),
    })
}

/// Run `command`, each line it writes handed to `said` as it comes;
/// stopped, with everything it started, once `cancel` is set.
pub fn run(
    command: &JsCommand,
    said: &mut dyn FnMut(&str, bool),
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<crate::spawn::RunEnd, String> {
    let mut cmd = crate::spawn::command(&command.program);
    cmd.args(&command.args).current_dir(&command.cwd);
    for (k, v) in &command.env {
        cmd.env(k, v);
    }
    // A runner that sees no terminal still colours its output where it is
    // told to: it is read here as text.
    cmd.env("NO_COLOR", "1").env("FORCE_COLOR", "0");
    crate::spawn::run_streaming(cmd, said, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A program the project installed, for a runner to be found.
    fn bin(root: &Path, rel_dir: &str, name: &str) -> PathBuf {
        let path = write(
            root,
            &format!("{rel_dir}/node_modules/.bin/{name}"),
            "#!/bin/sh\n",
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn a_titles_pattern_matches_what_the_runner_names_it() {
        assert_eq!(title_pattern("adds (two) items"), "adds \\(two\\) items");
        assert_eq!(title_pattern("adds %i + %i"), "adds .* \\+ .*");
        assert_eq!(title_pattern("`adds ${a} and ${b.c}`"), "adds .* and .*");
        assert_eq!(title_pattern("$name is valid"), ".* is valid");
        assert_eq!(title_pattern("100%% sure"), "100% sure");
        assert_eq!(
            name_pattern(&["cart".into(), "adds".into()], true),
            "(^| )cart adds$"
        );
        assert_eq!(name_pattern(&["cart".into()], false), "(^| )cart( |$)");
    }

    /// The runner a file's tests run with: the one it imports, else its
    /// package's, else a package's it sits in (a monorepo's root).
    #[test]
    fn a_files_runner_is_the_one_it_imports_or_its_package_uses() {
        let root = crate::testutil::TempDir::new("jsrun-runner");
        write(
            &root,
            "package.json",
            r#"{"devDependencies": {"vitest": "^3"}}"#,
        );
        write(
            &root,
            "packages/api/package.json",
            r#"{"scripts": {"test": "mocha"}}"#,
        );
        write(&root, "packages/web/package.json", r#"{"name": "web"}"#);
        let file = root.join("packages/web/src/a.test.ts");
        assert_eq!(
            test_runner(&root, &file, "test('x', () => {})"),
            Some(Runner::Vitest),
            "the root's"
        );
        let api = root.join("packages/api/test/a.test.js");
        assert_eq!(
            test_runner(&root, &api, "it('x', () => {})"),
            Some(Runner::Mocha),
            "its own first"
        );
        for (source, runner) in [
            ("import { test } from 'bun:test'", Runner::Bun),
            ("import test from \"node:test\"", Runner::Node),
            (
                "import { test, expect } from '@playwright/test'",
                Runner::Playwright,
            ),
            ("import { jest } from '@jest/globals'", Runner::Jest),
            ("Deno.test('x', () => {})", Runner::Deno),
        ] {
            assert_eq!(test_runner(&root, &file, source), Some(runner), "{source}");
        }
        let bare = crate::testutil::TempDir::new("jsrun-bare");
        write(&bare, "package.json", "{}");
        assert_eq!(
            test_runner(&bare, &bare.join("a.test.js"), "test('x')"),
            None
        );
        write(&bare, "bun.lock", "");
        assert_eq!(
            test_runner(&bare, &bare.join("a.test.js"), "test('x')"),
            Some(Runner::Bun)
        );
    }

    /// A test, a suite, or the file's tests, run by Vitest from where the
    /// project installed it, in its package's folder.
    #[cfg(unix)]
    #[test]
    fn a_test_runs_by_its_full_name_in_its_package() {
        let root = crate::testutil::TempDir::new("jsrun-test");
        write(
            &root,
            "package.json",
            r#"{"devDependencies": {"vitest": "^3"}}"#,
        );
        let vitest = bin(&root, ".", "vitest");
        let source =
            "describe('cart', () => {\n  it('adds %i', () => {})\n  it('drops', () => {})\n})\n";
        let file = write(&root, "src/cart.test.ts", source);
        let search = std::ffi::OsString::new();
        let (cmd, called) = test_command(&root, &file, "typescript", source, 2, &search).unwrap();
        assert_eq!(cmd.program, vitest);
        assert_eq!(cmd.cwd, root.to_path_buf());
        assert_eq!(
            cmd.args,
            ["run", "./src/cart.test.ts", "-t", "(^| )cart adds .*$"]
        );
        assert_eq!(called, "adds %i");
        let (cmd, called) = test_command(&root, &file, "typescript", source, 1, &search).unwrap();
        assert_eq!(
            cmd.args,
            ["run", "./src/cart.test.ts", "-t", "(^| )cart( |$)"]
        );
        assert_eq!(called, "cart");
        let (cmd, called) = test_command(
            &root,
            &file,
            "typescript",
            &format!("{source}\n// end\n"),
            6,
            &search,
        )
        .unwrap();
        assert_eq!(cmd.args, ["run", "./src/cart.test.ts"], "the file's tests");
        assert_eq!(called, "cart.test.ts");
        let err =
            test_command(&root, &file, "typescript", "const x = 1\n", 1, &search).unwrap_err();
        assert!(err.contains("no test here"), "{err}");
        std::fs::remove_file(&vitest).unwrap();
        let err = test_command(&root, &file, "typescript", source, 2, &search).unwrap_err();
        assert!(err.contains("Vitest is not installed"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn each_runner_is_asked_for_the_test_as_it_takes_it() {
        let root = crate::testutil::TempDir::new("jsrun-runners");
        let source = "describe('cart', () => {\n  it('adds', () => {})\n})\n";
        let file = write(&root, "test/cart.test.js", source);
        let search = std::ffi::OsString::new();
        let args = |manifest: &str, source: &str, bin_name: Option<&str>| {
            write(&root, "package.json", manifest);
            let installed = bin_name.map(|b| bin(&root, ".", b));
            let got =
                test_command(&root, &file, "javascript", source, 2, &search).map(|(c, _)| c.args);
            if let Some(p) = installed {
                std::fs::remove_file(p).unwrap();
            }
            got
        };
        assert_eq!(
            args(
                r#"{"devDependencies": {"jest": "^29"}}"#,
                source,
                Some("jest")
            )
            .unwrap(),
            [
                "--runTestsByPath",
                "./test/cart.test.js",
                "-t",
                "(^| )cart adds$"
            ]
        );
        assert_eq!(
            args(
                r#"{"dependencies": {"react-scripts": "5"}}"#,
                source,
                Some("react-scripts")
            )
            .unwrap(),
            [
                "test",
                "--watchAll=false",
                "--runTestsByPath",
                "./test/cart.test.js",
                "-t",
                "(^| )cart adds$"
            ]
        );
        assert_eq!(
            args(
                r#"{"devDependencies": {"mocha": "^10"}}"#,
                source,
                Some("mocha")
            )
            .unwrap(),
            ["./test/cart.test.js", "-g", "(^| )cart adds$"]
        );
        assert_eq!(
            args(
                r#"{"devDependencies": {"@playwright/test": "^1"}}"#,
                source,
                Some("playwright")
            )
            .unwrap(),
            ["test", "./test/cart.test.js:2"]
        );
        // Runtimes are looked for on the search path, which has none here.
        let err = args(
            "{}",
            &format!("import test from 'node:test'\n{source}"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("Node.js wasn't found"), "{err}");
    }

    /// A program runs by its project's runtime: Deno's, Bun's, or Node with
    /// TypeScript's types stripped, or the project's tsx (JSX needs it).
    #[cfg(unix)]
    #[test]
    fn a_program_runs_by_its_projects_runtime() {
        let root = crate::testutil::TempDir::new("jsrun-main");
        let runtimes = crate::testutil::TempDir::new("jsrun-runtimes");
        for name in ["node", "bun", "deno"] {
            let p = write(&runtimes, name, "#!/bin/sh\n");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let search = runtimes.as_os_str().to_owned();
        write(&root, "package.json", "{}");
        let ts = write(&root, "src/main.ts", "console.log(1)\n");
        let cmd = main_command(&root, &ts, &search).unwrap();
        assert_eq!(cmd.program, runtimes.join("node"));
        assert_eq!(cmd.args, ["--experimental-strip-types", "./src/main.ts"]);
        let jsx = write(&root, "src/App.jsx", "export default () => <p/>\n");
        assert!(
            main_command(&root, &jsx, &search)
                .unwrap_err()
                .contains("tsx")
        );
        let tsx = bin(&root, ".", "tsx");
        let cmd = main_command(&root, &jsx, &search).unwrap();
        assert_eq!(
            (cmd.program, cmd.args),
            (tsx.clone(), vec!["./src/App.jsx".to_string()])
        );
        std::fs::remove_file(&tsx).unwrap();
        write(&root, "bun.lock", "");
        let cmd = main_command(&root, &ts, &search).unwrap();
        assert_eq!(
            (cmd.program, cmd.args),
            (
                runtimes.join("bun"),
                vec!["run".to_string(), "./src/main.ts".to_string()]
            )
        );
        write(&root, "deno.json", "{}");
        let cmd = main_command(&root, &ts, &search).unwrap();
        assert_eq!(cmd.args, ["run", "--allow-all", "./src/main.ts"]);
    }

    /// A package's scripts, run by the package manager its lockfile (or its
    /// `packageManager`) names; only a script the package has.
    #[cfg(unix)]
    #[test]
    fn a_packages_scripts_run_by_its_package_manager() {
        let root = crate::testutil::TempDir::new("jsrun-scripts");
        let tools = crate::testutil::TempDir::new("jsrun-pms");
        for name in ["npm", "pnpm", "yarn", "bun"] {
            let p = write(&tools, name, "#!/bin/sh\n");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let search = tools.as_os_str().to_owned();
        write(
            &root,
            "package.json",
            r#"{"scripts": {"dev": "vite", "build": "vite build"}}"#,
        );
        assert_eq!(
            scripts(&root),
            [
                ("dev".to_string(), "vite".to_string()),
                ("build".to_string(), "vite build".to_string())
            ]
        );
        assert_eq!(package_manager(&root, &root), "npm");
        let cmd = script_command(&root, &root, "dev", &search).unwrap();
        assert_eq!(
            (cmd.program, cmd.args),
            (
                tools.join("npm"),
                vec!["run".to_string(), "dev".to_string()]
            )
        );
        assert!(
            script_command(&root, &root, "deploy", &search)
                .unwrap_err()
                .contains("no script")
        );
        write(&root, "pnpm-lock.yaml", "");
        assert_eq!(package_manager(&root, &root), "pnpm");
        write(
            &root,
            "packages/web/package.json",
            r#"{"packageManager": "yarn@4.1.0", "scripts": {"dev": "next"}}"#,
        );
        let web = root.join("packages/web");
        assert_eq!(
            package_manager(&root, &web),
            "yarn",
            "the package's own says first"
        );
        let cmd = script_command(&root, &web, "dev", &search).unwrap();
        assert_eq!((cmd.program, cmd.cwd), (tools.join("yarn"), web.clone()));
    }
}

#[cfg(test)]
mod live {
    /// A real Node runs a `node:test` file's test, suite and file, as
    /// `tasks::run` runs a JavaScript test on the project's host, and a
    /// failing test ends the run with a failure.
    #[test]
    #[ignore = "runs Node.js (on the search path); run explicitly"]
    fn live_node_runs_a_test_its_suite_and_its_file() {
        use std::sync::atomic::AtomicBool;
        let root = crate::testutil::TempDir::new("jsrun-live");
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("test")).unwrap();
        let source = "import { describe, it } from 'node:test'\nimport assert from 'node:assert'\n\ndescribe('cart', () => {\n  it('adds', () => { assert.equal(1 + 1, 2) })\n  it('drops', () => { assert.equal(1, 2) })\n})\n";
        std::fs::write(root.join("test/cart.test.mjs"), source).unwrap();
        let run = |line: usize| {
            let mut out = String::new();
            let end = crate::tasks::run(
                &root,
                &clew_protocol::ProjectTask::JsTest {
                    file: "test/cart.test.mjs".into(),
                    line,
                },
                &Default::default(),
                &mut |l, _| {
                    out.push_str(l);
                    out.push('\n');
                },
                &AtomicBool::new(false),
            )
            .unwrap();
            (end, out)
        };
        let (end, out) = run(5);
        println!("{out}");
        assert_eq!(end, crate::spawn::RunEnd::Exited(0), "{out}");
        assert!(out.contains("adds"), "{out}");
        assert!(!out.contains("✖ drops"), "only the test asked for: {out}");
        let (end, out) = run(4);
        assert_ne!(
            end,
            crate::spawn::RunEnd::Exited(0),
            "the suite's failing test: {out}"
        );
        let (end, _) = run(6);
        assert_ne!(end, crate::spawn::RunEnd::Exited(0));
    }
}
