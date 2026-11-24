//! The project's tasks, run where the project is ([`ProjectTask`]): a Lean
//! file's `main`, Lake's build and tests, Mathlib's build cache, a Rust
//! program, a Rust test. What runs is worked out here, on the project's
//! host, from what the task is — the program, its arguments, the folder it
//! runs in — and never from a command line the asker names: clew-server
//! runs them for a remote project ([`clew_protocol::Request::RunTask`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use clew_protocol::{LanguageChoices, ProjectTask};

use crate::cargorun::{self, CargoTarget, CargoTests, Features, TargetKind};
use crate::spawn::RunEnd;

/// Run `task` of the project at `root`: each line it writes handed to
/// `said` as it comes, with whether it was written to stderr; stopped, with
/// everything it started, once `cancel` is set. `Err` says why it did not
/// run — no Lean here, a file in no Cargo package, a build that failed.
/// Blocking.
pub fn run(
    root: &Path,
    task: &ProjectTask,
    choices: &LanguageChoices,
    said: &mut dyn FnMut(&str, bool),
    cancel: &AtomicBool,
) -> Result<RunEnd, String> {
    let rel = match task {
        ProjectTask::LeanMain { file }
        | ProjectTask::Lake { file, .. }
        | ProjectTask::MathlibCache { file }
        | ProjectTask::RustMain { file }
        | ProjectTask::RustTest { file, .. } => file,
    };
    let file = crate::confine::confine(root, rel).map_err(|e| format!("{rel}: {e}"))?;
    match task {
        ProjectTask::LeanMain { .. } => {
            let (lake, project) = lake_project(root, &file, choices)?;
            crate::lean::run_main(&lake, &project, &file, said, cancel)
        }
        ProjectTask::Lake { test, .. } => {
            let (lake, project) = lake_project(root, &file, choices)?;
            let args: &[&str] = if *test { &["test"] } else { &["build"] };
            crate::lean::run_lake(&lake, &project, args, said, cancel)
        }
        ProjectTask::MathlibCache { .. } => {
            let (lake, project) = lake_project(root, &file, choices)?;
            crate::lean::run_lake(&lake, &project, &["exe", "cache", "get"], said, cancel)
        }
        ProjectTask::RustMain { .. } => rust_main(root, &file, choices, said, cancel),
        ProjectTask::RustTest { line, .. } => rust_test(&file, *line, choices, said, cancel),
    }
}

/// The `lake` a Lean task runs with — the toolchain chosen for the project
/// (a program chosen for its Lean, run as `lake`), else the one on the
/// search path (elan's) — and the Lake project of `file`.
fn lake_project(
    root: &Path,
    file: &Path,
    choices: &LanguageChoices,
) -> Result<(PathBuf, PathBuf), String> {
    let chosen = choices
        .servers
        .as_ref()
        .and_then(|servers| servers.get("lean"))
        .filter(|choice| choice.args.is_none());
    let lake = match chosen {
        Some(choice) => crate::lsp::store::find_program(&choice.command)?,
        None => crate::lsp::store::find_on_path_unresolved("lake")
            .ok_or_else(|| "Lean's lake wasn't found here. Install Lean with elan.".to_string())?,
    };
    let project = crate::lean::lake_root(root, file).unwrap_or_else(|| root.to_path_buf());
    Ok((lake, project))
}

/// Cargo, or why there is none.
fn cargo() -> Result<PathBuf, String> {
    cargorun::find_cargo()
        .ok_or_else(|| "Cargo was not found here. Install Rust from rustup.rs.".to_string())
}

/// A Rust program built to run, or to debug: the executable, its
/// arguments, the folder it runs in, and the variables Cargo sets for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

/// Build what a Rust task runs — `RustMain`'s program, `RustTest`'s tests
/// (not a doc example: rustdoc runs those, with no debugger) — for the
/// debugger to start: Cargo's lines said as they come; `None` when
/// stopped. `Err` for another task, or what kept it from being built.
pub fn build_rust(
    root: &Path,
    task: &ProjectTask,
    choices: &LanguageChoices,
    said: &mut dyn FnMut(&str, bool),
    cancel: &AtomicBool,
) -> Result<Option<Built>, String> {
    let (rel, line) = match task {
        ProjectTask::RustMain { file } => (file, None),
        ProjectTask::RustTest { file, line } => (file, Some(*line)),
        _ => return Err("Only a Rust program or test is debugged here.".to_string()),
    };
    let file = crate::confine::confine(root, rel).map_err(|e| format!("{rel}: {e}"))?;
    match line {
        None => main_built(root, &file, choices, said, cancel),
        Some(line) => {
            let source = crate::statefile::read_capped(&file, 8 << 20)
                .ok_or_else(|| "The file could not be read.".to_string())?;
            let found = crate::runnables::runnables("rust", &source);
            if crate::runnables::runnable_at(&found, line)
                .is_some_and(|r| r.kind == crate::runnables::RunnableKind::Doctest)
            {
                return Err("A doc example is run by rustdoc, with no debugger.".to_string());
            }
            tests_built(&file, &source, line, choices, said, cancel)
        }
    }
}

/// The program `file` is a source of, built and run as `cargo run` runs
/// it, from the project's folder: its binary's (an example's, a test's, a
/// bench's own `main`), or — a library's file — its package's one program.
fn rust_main(
    root: &Path,
    file: &Path,
    choices: &LanguageChoices,
    said: &mut dyn FnMut(&str, bool),
    cancel: &AtomicBool,
) -> Result<RunEnd, String> {
    match main_built(root, file, choices, said, cancel)? {
        Some(built) => run_built(&built, said, cancel),
        None => Ok(RunEnd::Stopped),
    }
}

/// [`rust_main`]'s program, built.
fn main_built(
    root: &Path,
    file: &Path,
    choices: &LanguageChoices,
    said: &mut dyn FnMut(&str, bool),
    cancel: &AtomicBool,
) -> Result<Option<Built>, String> {
    let cargo = cargo()?;
    let target =
        cargorun::target_of(file).ok_or_else(|| "This file is in no Cargo package.".to_string())?;
    let target = match target.kind {
        TargetKind::Lib => match cargorun::package_program(&target.manifest) {
            cargorun::PackageProgram::One(program) => program,
            cargorun::PackageProgram::None => {
                return Err(format!(
                    "{} is a library, with no program to run.",
                    target.package
                ));
            }
            cargorun::PackageProgram::Several(names) => {
                return Err(format!(
                    "{} has several programs ({}), and no default-run to say which to run.",
                    target.package,
                    names.join(", ")
                ));
            }
        },
        _ => CargoTarget {
            module: Vec::new(),
            ..target
        },
    };
    let features = Features::from_choices(choices.rust.as_ref());
    let Some(built) = build(&cargo, &target, false, &features, said, cancel)? else {
        return Ok(None);
    };
    // A bench runs as `cargo bench` runs it: told it is benchmarking.
    let args = match target.kind {
        TargetKind::Bench(_) => vec!["--bench".to_string()],
        _ => Vec::new(),
    };
    Ok(Some(Built {
        program: built.executable,
        args,
        cwd: root.to_path_buf(),
        env: built.env,
    }))
}

/// The test, or the tests, at 1-based `line` of `file`, built and run as
/// Run Test runs them, in their package's folder; a doc example, by
/// rustdoc.
fn rust_test(
    file: &Path,
    line: usize,
    choices: &LanguageChoices,
    said: &mut dyn FnMut(&str, bool),
    cancel: &AtomicBool,
) -> Result<RunEnd, String> {
    use crate::runnables::{RunnableKind, runnable_at, runnables};
    let cargo = cargo()?;
    let features = Features::from_choices(choices.rust.as_ref());
    let source = crate::statefile::read_capped(file, 8 << 20)
        .ok_or_else(|| "The file could not be read.".to_string())?;
    let found = runnables("rust", &source);
    if runnable_at(&found, line).is_some_and(|r| r.kind == RunnableKind::Doctest) {
        let target = cargorun::target_of(file)
            .filter(|t| t.kind == TargetKind::Lib)
            .ok_or_else(|| "Only a library's doc examples are tests.".to_string())?;
        return match cargorun::run_doctest(&cargo, &target, &features, file, line, said, cancel)? {
            cargorun::DoctestEnd::Ran(end) => Ok(end),
            cargorun::DoctestEnd::NotListed => {
                Err("rustdoc lists no doc example there.".to_string())
            }
        };
    }
    match tests_built(file, &source, line, choices, said, cancel)? {
        Some(built) => run_built(&built, said, cancel),
        None => Ok(RunEnd::Stopped),
    }
}

/// [`rust_test`]'s tests, built, with the arguments that pick them.
fn tests_built(
    file: &Path,
    source: &str,
    line: usize,
    choices: &LanguageChoices,
    said: &mut dyn FnMut(&str, bool),
    cancel: &AtomicBool,
) -> Result<Option<Built>, String> {
    let cargo = cargo()?;
    let features = Features::from_choices(choices.rust.as_ref());
    let (target, runs, name) = cargorun::tests_at(file, source, line)
        .ok_or_else(|| "No test here. Put the cursor in a test function to run it.".to_string())?;
    let Some(built) = build(&cargo, &target, true, &features, said, cancel)? else {
        return Ok(None);
    };
    let package = target.package_dir().to_path_buf();
    let list = cargorun::list_tests(&built.executable, &package, &built.env)
        .ok_or_else(|| format!("Built the tests of {name}, but couldn't list them."))?;
    let args = match &runs {
        CargoTests::One(path) => cargorun::one_test_args(&list, path)
            .ok_or_else(|| format!("When built, there is no test named {name}."))?,
        CargoTests::Under(under) => {
            let selected = cargorun::listed_under(&list, under);
            if selected.is_empty() {
                return Err(format!("When built, there are no tests in {name}."));
            }
            cargorun::group_args(&list, under, &selected)
        }
    };
    Ok(Some(Built {
        program: built.executable,
        args,
        cwd: package,
        env: built.env,
    }))
}

/// Build `target` with `cargo` (its tests, `test`), Cargo's lines said as
/// stderr's; `None` when stopped.
fn build(
    cargo: &Path,
    target: &CargoTarget,
    test: bool,
    features: &Features,
    said: &mut dyn FnMut(&str, bool),
    cancel: &AtomicBool,
) -> Result<Option<cargorun::Built>, String> {
    match cargorun::build(cargo, target, test, features, |l| said(l, true), cancel) {
        Ok(built) => Ok(Some(built)),
        Err(cargorun::BuildError::Stopped) => Ok(None),
        Err(cargorun::BuildError::Spawn(why)) => Err(format!("Cargo could not be started: {why}")),
        Err(cargorun::BuildError::Failed) => Err("The build failed.".to_string()),
        Err(cargorun::BuildError::NoProgram) => {
            Err("Cargo built, and reported no program to run.".to_string())
        }
    }
}

/// Run what Cargo built, with its arguments, in its folder, with the
/// variables Cargo sets for it.
fn run_built(
    built: &Built,
    said: &mut dyn FnMut(&str, bool),
    cancel: &AtomicBool,
) -> Result<RunEnd, String> {
    let mut command = crate::spawn::command(&built.program);
    command
        .args(&built.args)
        .current_dir(&built.cwd)
        .envs(built.env.iter().map(|(k, v)| (k, v)));
    crate::spawn::run_streaming(command, said, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A task's file is the project's: one outside it, or a path that
    /// leaves it, runs nothing.
    #[test]
    fn a_tasks_file_is_confined_to_the_project() {
        let dir = crate::testutil::TempDir::new("tasks-confined");
        let root = dir.to_path_buf();
        std::fs::write(root.join("Main.lean"), "def main : IO Unit := pure ()\n").unwrap();
        let mut said = |_: &str, _: bool| {};
        for rel in ["../Main.lean", "/etc/passwd", ""] {
            let task = ProjectTask::LeanMain { file: rel.into() };
            let ran = run(
                &root,
                &task,
                &LanguageChoices::default(),
                &mut said,
                &AtomicBool::new(false),
            );
            assert!(ran.is_err(), "{rel}: {ran:?}");
        }
    }

    /// A Lean task with no Lean here says so; a Rust one in no Cargo
    /// package says so.
    #[test]
    fn a_task_that_cannot_run_says_why() {
        let dir = crate::testutil::TempDir::new("tasks-why");
        let root = dir.to_path_buf();
        std::fs::write(root.join("Main.lean"), "def main : IO Unit := pure ()\n").unwrap();
        std::fs::write(root.join("loose.rs"), "fn main() {}\n").unwrap();
        let _no_lean = crate::testutil::EnvVars::new()
            .set("ELAN_HOME", root.join("no-elan"))
            .set("PATH", root.join("no-bin"));
        let mut said = |_: &str, _: bool| {};
        let lean = run(
            &root,
            &ProjectTask::LeanMain {
                file: "Main.lean".into(),
            },
            &LanguageChoices::default(),
            &mut said,
            &AtomicBool::new(false),
        );
        assert!(lean.as_ref().is_err_and(|e| e.contains("lake")), "{lean:?}");
        let rust = run(
            &root,
            &ProjectTask::RustTest {
                file: "loose.rs".into(),
                line: 1,
            },
            &LanguageChoices::default(),
            &mut said,
            &AtomicBool::new(false),
        );
        assert!(rust.is_err(), "{rust:?}");
    }

    /// A Rust program, one test and a module's tests run as tasks, through
    /// the real Cargo (`CLEW_TEST_CARGO_TASKS` set).
    #[test]
    #[ignore = "builds with Cargo (CLEW_TEST_CARGO_TASKS=1); run explicitly"]
    fn live_rust_programs_and_tests_run_as_tasks() {
        let dir = crate::testutil::TempDir::new("tasks-rust-live");
        let root = dir.to_path_buf();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            "Cargo.toml",
            "[package]\nname = \"shapes\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        );
        write(
            "src/main.rs",
            "fn main() {\n    println!(\"area {}\", shapes::area(2));\n}\n",
        );
        write(
            "src/lib.rs",
            "pub fn area(x: u32) -> u32 { x * x }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn squares() {\n        assert_eq!(super::area(3), 9);\n    }\n\n    #[test]\n    fn fails() {\n        assert_eq!(super::area(1), 2);\n    }\n}\n",
        );
        let task = |task: ProjectTask| {
            let mut lines = Vec::new();
            let end = run(
                &root,
                &task,
                &LanguageChoices::default(),
                &mut |line, _| lines.push(line.to_string()),
                &AtomicBool::new(false),
            );
            (end, lines)
        };
        let (end, lines) = task(ProjectTask::RustMain {
            file: "src/lib.rs".into(),
        });
        assert_eq!(end, Ok(RunEnd::Exited(0)), "{lines:?}");
        assert!(lines.iter().any(|l| l == "area 4"), "{lines:?}");
        let (end, lines) = task(ProjectTask::RustTest {
            file: "src/lib.rs".into(),
            line: 6,
        });
        assert_eq!(end, Ok(RunEnd::Exited(0)), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("1 passed")), "{lines:?}");
        let (end, lines) = task(ProjectTask::RustTest {
            file: "src/lib.rs".into(),
            line: 4,
        });
        assert_eq!(end, Ok(RunEnd::Exited(101)), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("1 failed")), "{lines:?}");
        // Built for the debugger: the test binary, the one test picked, in
        // its package's folder, with Cargo's variables.
        let built = build_rust(
            &root,
            &ProjectTask::RustTest {
                file: "src/lib.rs".into(),
                line: 6,
            },
            &LanguageChoices::default(),
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap()
        .expect("built");
        assert!(built.program.is_file(), "{built:?}");
        assert!(
            built.args.iter().any(|a| a.contains("squares")),
            "{built:?}"
        );
        assert!(
            built.env.iter().any(|(k, _)| k == "CARGO_MANIFEST_DIR"),
            "{built:?}"
        );
        let main = build_rust(
            &root,
            &ProjectTask::RustMain {
                file: "src/main.rs".into(),
            },
            &LanguageChoices::default(),
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap()
        .expect("built");
        assert_eq!(main.cwd, root);
        assert!(
            build_rust(
                &root,
                &ProjectTask::LeanMain {
                    file: "src/main.rs".into()
                },
                &LanguageChoices::default(),
                &mut |_, _| {},
                &AtomicBool::new(false),
            )
            .is_err()
        );
    }

    /// A Lean `main` importing its project runs through the real Lake
    /// (`CLEW_TEST_LAKE`), as a task.
    #[test]
    #[ignore = "needs Lean's lake (CLEW_TEST_LAKE=path/to/lake); run explicitly"]
    fn live_a_lean_main_runs_as_a_task() {
        let lake = PathBuf::from(std::env::var("CLEW_TEST_LAKE").expect("CLEW_TEST_LAKE"));
        let dir = crate::testutil::TempDir::new("tasks-lean-live");
        let root = dir.to_path_buf();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            "proofs/lakefile.toml",
            "name = \"demo\"\n\n[[lean_lib]]\nname = \"Demo\"\n",
        );
        write("proofs/Demo/Basic.lean", "def two : Nat := 2\n");
        write(
            "proofs/Main.lean",
            "import Demo.Basic\n\ndef main : IO Unit :=\n  IO.println s!\"two is {two}\"\n",
        );
        let mut servers = std::collections::BTreeMap::new();
        servers.insert(
            "lean".to_string(),
            clew_protocol::ServerChoice {
                command: lake.to_string_lossy().into_owned(),
                args: None,
            },
        );
        let choices = LanguageChoices {
            servers: Some(servers),
            ..Default::default()
        };
        let mut lines = Vec::new();
        let end = run(
            &root,
            &ProjectTask::LeanMain {
                file: "proofs/Main.lean".into(),
            },
            &choices,
            &mut |line, _| lines.push(line.to_string()),
            &AtomicBool::new(false),
        );
        assert_eq!(end, Ok(RunEnd::Exited(0)), "{lines:?}");
        assert!(lines.iter().any(|l| l == "two is 2"), "{lines:?}");
    }
}
