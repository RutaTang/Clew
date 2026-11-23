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
    /// Python tests, run by pytest by their node id: one test (or one
    /// case of a parametrized one), a test class's, or a test file's.
    Pytest {
        file: PathBuf,
        node: String,
        /// What the run is called: the test's name, its class's, or its
        /// file's.
        name: String,
        tested: Tested,
    },
    /// One test of a Django project, run by Django's own test runner
    /// (`manage.py test <label>`), where pytest has no Django settings to
    /// run it with.
    Django {
        /// The test's file.
        file: PathBuf,
        /// The project's `manage.py`.
        manage: PathBuf,
        /// The test's dotted label (`shop.tests.TestCart.test_total`), its
        /// class's or its module's.
        label: String,
        name: String,
        tested: Tested,
    },
    /// A Python program's start (`if __name__ == "__main__":`), its file
    /// run as a script: what the code's own Run mark runs, whatever a
    /// launch file names.
    Script { file: PathBuf },
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

/// How much of a test file a test run runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tested {
    /// One test, or one case of a parametrized test.
    One,
    /// A group of tests: a test class's, or a test file's.
    Group,
}

/// Rust tests of a Cargo target: one, or a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoTest {
    /// Which, by the names its test binary lists them under.
    pub runs: CargoTests,
    /// What the run is called: the test's function's name (`adds`), its
    /// module's (`tests`), or its file's (`parser.rs`).
    pub name: String,
}

/// Which tests of a test binary a run runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CargoTests {
    /// One test, by its name as worked out from the code
    /// (`parser::tests::adds`).
    One(String),
    /// A group: the tests named one of these or below it — a module
    /// (`parser::tests`), a test at its file's root — every test of the
    /// target where there are none.
    Under(Vec<String>),
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
            RunPlan::Pytest { name, .. } | RunPlan::Django { name, .. } => name.clone(),
            RunPlan::Script { file } | RunPlan::Lean { file, .. } => file
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
            RunPlan::Pytest { name, .. } | RunPlan::Django { name, .. } => Some(name),
            RunPlan::Cargo { test: None, .. } | RunPlan::Script { .. } | RunPlan::Lean { .. } => {
                None
            }
        }
    }

    /// What the end of a run of one test says, by its exit `code`: that it
    /// passed or failed — or, for pytest, that it found no such test (5),
    /// could not load it (2: an import or collection error), or stopped
    /// with an error of its own (3, 4): the test never ran, and "failed"
    /// said it had. A group's run says the same of its tests. `None` for
    /// a run of anything but tests.
    pub(crate) fn verdict(&self, code: i64) -> Option<String> {
        let name = self.test_name()?;
        let group = matches!(
            self,
            RunPlan::Pytest {
                tested: Tested::Group,
                ..
            } | RunPlan::Django {
                tested: Tested::Group,
                ..
            } | RunPlan::Cargo {
                test: Some(CargoTest {
                    runs: CargoTests::Under(_),
                    ..
                }),
                ..
            }
        );
        if group {
            return Some(match (self, code) {
                (_, 0) => tr!("Tests in {name} passed", name = name),
                (RunPlan::Pytest { .. }, 5) => tr!("pytest found no tests in {name}", name = name),
                (RunPlan::Pytest { .. }, 2) => tr!(
                    "pytest could not load {name} (see the run's output)",
                    name = name
                ),
                (RunPlan::Pytest { .. }, 3 | 4) => tr!(
                    "pytest stopped with an error before the tests in {name} ran (see the run's output)",
                    name = name
                ),
                _ => tr!("Some tests in {name} failed", name = name),
            });
        }
        Some(match (self, code) {
            (_, 0) => tr!("Test {name} passed", name = name),
            (RunPlan::Pytest { .. }, 5) => tr!("pytest found no test named {name}", name = name),
            (RunPlan::Pytest { .. }, 2) => tr!(
                "pytest could not load {name} (see the run's output)",
                name = name
            ),
            (RunPlan::Pytest { .. }, 3 | 4) => tr!(
                "pytest stopped with an error before {name} ran (see the run's output)",
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
            RunPlan::Pytest { .. }
            | RunPlan::Django { .. }
            | RunPlan::Script { .. }
            | RunPlan::Lean { .. } => false,
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
            // In the folder of its `manage.py`, as Django's tests are run.
            RunPlan::Django { manage, label, .. } => (
                manage.clone(),
                vec!["test".into(), label.clone(), "--noinput".into()],
                manage
                    .parent()
                    .map_or_else(|| root.to_path_buf(), Path::to_path_buf),
                Some(crate::dap::Lang::Python),
            ),
            RunPlan::Script { file } => (
                file.clone(),
                Vec::new(),
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
            RunPlan::Pytest { file, .. }
            | RunPlan::Django { file, .. }
            | RunPlan::Script { file }
            | RunPlan::Lean { file, .. } => {
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
    /// It is a library's, and the package has these programs, with no
    /// `default-run` to say which `cargo run` runs.
    Several(Vec<String>),
}

/// What Run runs for the Rust file `file`: the program it is compiled in —
/// a binary or an example — or a test target's tests; a library's module,
/// the program `cargo run` runs in its package (its `default-run`, or its
/// only binary, wherever Cargo finds it).
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
            use clew_core::cargorun::PackageProgram;
            match clew_core::cargorun::package_program(&target.manifest) {
                PackageProgram::One(program) => Ok(RunPlan::Cargo {
                    target: program,
                    test: None,
                }),
                PackageProgram::None => Err(NoProgram::Library),
                PackageProgram::Several(names) => Err(NoProgram::Several(names)),
            }
        }
    }
}

/// The test at 1-based `line` of `file` (its text `source`, in language
/// `lang`), as the run that runs it alone: a Rust test through Cargo, a
/// Python one by pytest. `None` where the line is in no test.
pub(crate) fn test_plan_at(lang: &str, file: &Path, source: &str, line: usize) -> Option<RunPlan> {
    let found = runnables(lang, source);
    let mut test = runnable_at(&found, line)
        .filter(|r| matches!(r.kind, RunnableKind::Test | RunnableKind::Tests))?;
    match lang {
        "rust" => {
            let target = clew_core::cargorun::target_of(file)?;
            let runs = match (test.kind, test.name.is_empty()) {
                (RunnableKind::Test, _) => {
                    CargoTests::One(target.test_path(&test.scopes, &test.name))
                }
                (_, false) => CargoTests::Under(vec![target.test_path(&test.scopes, &test.name)]),
                // A file's tests: its module's; a test target's root file
                // holds every test of it; a library's or a program's root,
                // its own modules' and tests' — not the crate's every test.
                (_, true) if !target.module.is_empty() => {
                    CargoTests::Under(vec![target.module.join("::")])
                }
                (_, true) if matches!(target.kind, TargetKind::Test(_)) => {
                    CargoTests::Under(Vec::new())
                }
                (_, true) => CargoTests::Under(
                    found
                        .iter()
                        .filter(|r| {
                            r.scopes.is_empty()
                                && !r.name.is_empty()
                                && matches!(r.kind, RunnableKind::Test | RunnableKind::Tests)
                        })
                        .map(|r| r.name.clone())
                        .collect(),
                ),
            };
            Some(RunPlan::Cargo {
                test: Some(CargoTest {
                    runs,
                    name: match test.name.is_empty() {
                        true => file_name(file),
                        false => test.name.clone(),
                    },
                }),
                target,
            })
        }
        "python" => {
            // A case whose id the project's own hook may rename: the whole
            // test, which is found whatever its cases are called.
            if test.name.contains('[') && renames_cases(file) {
                let whole = test.name.split('[').next().unwrap_or_default();
                test = found
                    .iter()
                    .find(|r| {
                        r.kind == RunnableKind::Test && r.name == whole && r.scopes == test.scopes
                    })
                    .unwrap_or(test);
            }
            let tested = match test.kind {
                RunnableKind::Tests => Tested::Group,
                _ => Tested::One,
            };
            let name = match test.name.is_empty() {
                true => file_name(file),
                false => test.name.clone(),
            };
            let classes = found
                .iter()
                .any(|r| r.kind == RunnableKind::Test && !r.scopes.is_empty());
            Some(
                django_plan(file, source, test, classes).unwrap_or_else(|| RunPlan::Pytest {
                    file: file.to_path_buf(),
                    node: pytest_node(&file.to_string_lossy(), test),
                    name,
                    tested,
                }),
            )
        }
        _ => None,
    }
}

/// The program whose start is at 1-based `line` of `file` (its text
/// `source`, in language `lang`), as the run that runs it: a Rust file's
/// `main`, its program built by Cargo; a Python file's `__main__` block,
/// the file run as a script. `None` where no program starts there.
pub(crate) fn main_plan_at(lang: &str, file: &Path, source: &str, line: usize) -> Option<RunPlan> {
    let found = runnables(lang, source);
    runnable_at(&found, line).filter(|r| r.kind == RunnableKind::Main)?;
    match lang {
        "rust" => cargo_plan_for(file).ok(),
        "python" => Some(RunPlan::Script {
            file: file.to_path_buf(),
        }),
        _ => None,
    }
}

/// A file's name, as a run of its tests is called (`test_cart.py`).
fn file_name(file: &Path) -> String {
    file.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Whether a `conftest.py` above `file` names parametrized cases its own
/// way (`pytest_make_parametrize_id`), so that a case's id is not the one
/// read from the test.
pub(crate) fn renames_cases(file: &Path) -> bool {
    file.ancestors()
        .skip(1)
        .take(MAX_MANAGE_DEPTH)
        .filter_map(|dir| {
            clew_core::statefile::read_capped(&dir.join("conftest.py"), MAX_CONFIG_BYTES)
        })
        .any(|text| text.contains("pytest_make_parametrize_id"))
}

/// Most folders above a test file looked in for its project's `manage.py`.
const MAX_MANAGE_DEPTH: usize = 8;

/// Most bytes of a configuration file read for its pytest-django setup.
const MAX_CONFIG_BYTES: u64 = 512 * 1024;

/// Tests of a Django project's, run as Django runs its tests
/// (`manage.py test`): a test method of a class in a file that uses
/// Django, its class's, or its module's when it has test classes
/// (`classes`), in a project with a `manage.py` above it whose pytest is
/// not set up for Django (pytest-django, `DJANGO_SETTINGS_MODULE`) — under
/// pytest alone, Django's settings were never configured and its tests
/// failed before they ran.
fn django_plan(
    file: &Path,
    source: &str,
    test: &clew_core::runnables::Runnable,
    classes: bool,
) -> Option<RunPlan> {
    let unittest = match test.kind {
        RunnableKind::Tests if test.name.is_empty() => classes,
        RunnableKind::Tests => true,
        _ => !test.scopes.is_empty() && !test.name.contains('['),
    };
    if !unittest || !source.contains("django") {
        return None;
    }
    let project = file
        .ancestors()
        .skip(1)
        .take(MAX_MANAGE_DEPTH)
        .find(|dir| dir.join("manage.py").is_file())?;
    if pytest_runs_django(project) {
        return None;
    }
    let module = file.strip_prefix(project).ok()?.with_extension("");
    let mut parts: Vec<String> = module
        .components()
        .map(|c| c.as_os_str().to_str().map(str::to_string))
        .collect::<Option<_>>()?;
    if parts.last().is_some_and(|p| p == "__init__") {
        parts.pop();
    }
    parts.extend(test.scopes.iter().cloned());
    if !test.name.is_empty() {
        parts.push(test.name.clone());
    }
    Some(RunPlan::Django {
        file: file.to_path_buf(),
        manage: project.join("manage.py"),
        label: parts.join("."),
        name: match test.name.is_empty() {
            true => file_name(file),
            false => test.name.clone(),
        },
        tested: match test.kind {
            RunnableKind::Tests => Tested::Group,
            _ => Tested::One,
        },
    })
}

/// Whether the project at `dir` has pytest set up to run its Django tests
/// (pytest-django, with the settings named): then pytest runs them.
fn pytest_runs_django(dir: &Path) -> bool {
    [
        "pytest.ini",
        "pyproject.toml",
        "setup.cfg",
        "tox.ini",
        "conftest.py",
    ]
    .iter()
    .filter_map(|f| clew_core::statefile::read_capped(&dir.join(f), MAX_CONFIG_BYTES))
    .any(|text| {
        text.contains("DJANGO_SETTINGS_MODULE")
            || text.contains("pytest_django")
            || text.contains("pytest-django")
    })
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
