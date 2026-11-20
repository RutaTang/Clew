//! Running what is in the code as its language's tools run it: a Rust
//! program built by Cargo — what Run and Debug do on a Rust file with no
//! launch file — and one test, Rust's or Python's, at a line (Run Test,
//! Debug Test).
//!
//! A run asked for so keeps how it was asked for ([`RunPlan`]), so Run Again
//! runs the same test, not whatever file is open by then.

use std::path::{Path, PathBuf};

use crate::tr;
use clew_core::cargorun::{CargoTarget, TargetKind};
use clew_core::runnables::{RunnableKind, pytest_node, runnable_at, runnables};

/// How a run was asked for, beyond its program: kept with the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunPlan {
    /// A Cargo target, built first, then run: a program (a binary, an
    /// example), or its test binary — all of its tests, or the one named.
    Cargo {
        target: CargoTarget,
        test: Option<CargoTest>,
    },
    /// One Python test, run by pytest by its node id.
    Pytest {
        file: PathBuf,
        node: String,
        name: String,
    },
    /// A Lean file's `main`, run by Lean in its Lake project (`lake env
    /// lean --run`): no debugger, its output and how it ended.
    Lean {
        file: PathBuf,
        /// Its Lake project: the folder of the nearest lakefile.
        project: PathBuf,
        /// The `lake` it runs with.
        lake: PathBuf,
    },
}

/// One Rust test of a Cargo target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoTest {
    /// Its name in its test binary as worked out from the code
    /// (`parser::tests::adds`).
    pub path: String,
    /// Its function's name (`adds`).
    pub name: String,
}

impl RunPlan {
    /// What the run is called: its tab, the status line.
    pub(crate) fn label(&self) -> String {
        match self {
            RunPlan::Cargo {
                test: Some(test), ..
            } => test.name.clone(),
            RunPlan::Cargo { target, test: None } => match &target.kind {
                TargetKind::Test(name) => tr!("{target} tests", target = name),
                _ => target_name(target),
            },
            RunPlan::Pytest { name, .. } => name.clone(),
            RunPlan::Lean { file, .. } => file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }

    /// The one test it runs, by its function's name.
    pub(crate) fn test_name(&self) -> Option<&str> {
        match self {
            RunPlan::Cargo {
                test: Some(test), ..
            } => Some(&test.name),
            RunPlan::Pytest { name, .. } => Some(name),
            RunPlan::Cargo { test: None, .. } | RunPlan::Lean { .. } => None,
        }
    }

    /// What the end of a run of one test says, by its exit `code`: that it
    /// passed or failed — or, for pytest, that it found no such test (5),
    /// could not load it (2: an import or collection error), or stopped
    /// with an error of its own (3, 4): the test never ran, and "failed"
    /// said it had. `None` for a run of anything but one test.
    pub(crate) fn verdict(&self, code: i64) -> Option<String> {
        let name = self.test_name()?;
        Some(match (self, code) {
            (_, 0) => tr!("Test {name} passed", name = name),
            (RunPlan::Pytest { .. }, 5) => tr!("pytest found no test named {name}", name = name),
            (RunPlan::Pytest { .. }, 2) => tr!(
                "pytest could not load {name}: what it said is in the run's output",
                name = name
            ),
            (RunPlan::Pytest { .. }, 3 | 4) => tr!(
                "pytest stopped with an error before {name} ran: what it said is in the run's output",
                name = name
            ),
            _ => tr!("Test {name} failed", name = name),
        })
    }

    /// Whether Cargo builds tests for it: one test, or a test target's all.
    pub(crate) fn builds_tests(&self) -> bool {
        match self {
            RunPlan::Cargo { target, test } => {
                test.is_some() || matches!(target.kind, TargetKind::Test(_))
            }
            RunPlan::Pytest { .. } | RunPlan::Lean { .. } => false,
        }
    }

    /// What the debugger is given, for a project at `root`: the program,
    /// its arguments, the folder it runs in, and its language. A Cargo
    /// plan's program is built first, and these stand in for it until then
    /// (`App::start_session`'s build): its package's folder, no arguments.
    /// A test runs in its package's folder, as `cargo test` runs it; a
    /// program in the project's, as `cargo run` from it; a Python test in
    /// the project's, where pytest finds its configuration.
    /// A Lean file's run has no debugger: no language.
    pub(crate) fn launch(
        &self,
        root: &Path,
    ) -> (PathBuf, Vec<String>, PathBuf, Option<crate::dap::Lang>) {
        match self {
            RunPlan::Cargo { target, .. } => {
                let package = target.package_dir().to_path_buf();
                let cwd = match self.builds_tests() {
                    true => package.clone(),
                    false => root.to_path_buf(),
                };
                (package, Vec::new(), cwd, Some(crate::dap::Lang::Native))
            }
            RunPlan::Pytest { file, node, .. } => (
                file.clone(),
                vec![node.clone()],
                root.to_path_buf(),
                Some(crate::dap::Lang::Python),
            ),
            RunPlan::Lean { file, project, .. } => {
                (file.clone(), Vec::new(), project.clone(), None)
            }
        }
    }

    /// The run's name as a path, its last part what its tab and the status
    /// line call it ([`Self::label`]): beside its package or its file, so
    /// runs of different packages' `main` are told apart.
    pub(crate) fn shown(&self) -> PathBuf {
        let beside = match self {
            RunPlan::Cargo { target, .. } => target.package_dir().to_path_buf(),
            RunPlan::Pytest { file, .. } | RunPlan::Lean { file, .. } => {
                file.parent().map(Path::to_path_buf).unwrap_or_default()
            }
        };
        beside.join(self.label())
    }
}

/// Why a Rust file has nothing Run can run on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoProgram {
    /// The file is in no Cargo package Clew can read.
    NoPackage,
    /// It is a library's, and the package has no program to run.
    Library,
}

/// What Run runs for the Rust file `file`: the program it is compiled in —
/// a binary or an example — or a test target's tests; a library's module,
/// the package's own program when it has one (`src/main.rs`).
pub(crate) fn cargo_plan_for(file: &Path) -> Result<RunPlan, NoProgram> {
    let target = clew_core::cargorun::target_of(file).ok_or(NoProgram::NoPackage)?;
    match target.kind {
        TargetKind::Bin(_) | TargetKind::Example(_) | TargetKind::Test(_) => Ok(RunPlan::Cargo {
            target: CargoTarget {
                module: Vec::new(),
                ..target
            },
            test: None,
        }),
        TargetKind::Lib | TargetKind::Bench(_) => {
            let main = target.package_dir().join("src").join("main.rs");
            let program = main
                .is_file()
                .then(|| clew_core::cargorun::target_of(&main))
                .flatten()
                .filter(|t| t.runs())
                .ok_or(NoProgram::Library)?;
            Ok(RunPlan::Cargo {
                target: program,
                test: None,
            })
        }
    }
}

/// The test at 1-based `line` of `file` (its text `source`, in language
/// `lang`), as the run that runs it alone: a Rust test through Cargo, a
/// Python one by pytest. `None` where the line is in no test.
pub(crate) fn test_plan_at(lang: &str, file: &Path, source: &str, line: usize) -> Option<RunPlan> {
    let found = runnables(lang, source);
    let test = runnable_at(&found, line).filter(|r| r.kind == RunnableKind::Test)?;
    match lang {
        "rust" => {
            let target = clew_core::cargorun::target_of(file)?;
            Some(RunPlan::Cargo {
                test: Some(CargoTest {
                    path: target.test_path(&test.scopes, &test.name),
                    name: test.name.clone(),
                }),
                target,
            })
        }
        "python" => Some(RunPlan::Pytest {
            file: file.to_path_buf(),
            node: pytest_node(&file.to_string_lossy(), test),
            name: test.name.clone(),
        }),
        _ => None,
    }
}

/// A Cargo target as a sentence names it, in the reader's language: the
/// program by its name, the others as what they are ("the demo example",
/// "the api test target", "acme (library)").
pub(crate) fn target_name(target: &clew_core::cargorun::CargoTarget) -> String {
    match &target.kind {
        TargetKind::Lib => tr!("{package} (library)", package = target.package),
        TargetKind::Bin(name) => name.clone(),
        TargetKind::Example(name) => tr!("the {name} example", name = name),
        TargetKind::Test(name) => tr!("the {name} test target", name = name),
        TargetKind::Bench(name) => tr!("the {name} benchmark", name = name),
    }
}
