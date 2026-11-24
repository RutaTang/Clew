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
    /// One example of a Rust doc comment, run by rustdoc as `cargo test
    /// --doc` runs it, alone: no debugger (rustdoc builds and runs it
    /// itself), its output and how it ended.
    Doctest {
        /// Its package's library.
        target: CargoTarget,
        /// The file its comment is in.
        file: PathBuf,
        /// The 1-based line its example opens on.
        line: usize,
        /// The item it documents (`add`); empty for a module's own.
        item: String,
    },
    /// A task run where a remote project is, by its clew-server
    /// (`RunTask`): a Lean `main`, Lake's build or tests, a Rust program or
    /// test. No debugger: its output and how it ended.
    Remote {
        task: clew_protocol::ProjectTask,
        /// What the run is called: the file, the test, the Lake task.
        name: String,
        /// For tests: whether a group's (`true`) or one test's.
        tests: Option<bool>,
    },
    /// A Lake project built, or its tests run (`lake build`, `lake test`):
    /// no debugger, its output and how it ended.
    Lake {
        /// The Lake project: the folder of its lakefile.
        project: PathBuf,
        /// The `lake` it runs with.
        lake: PathBuf,
        task: LakeTask,
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

/// What Lake is asked to do for a project ([`RunPlan::Lake`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LakeTask {
    /// `lake build`: the project's default targets.
    Build,
    /// `lake test`: its test driver.
    Test,
}

impl LakeTask {
    /// Lake's words for it.
    pub(crate) fn args(self) -> &'static [&'static str] {
        match self {
            LakeTask::Build => &["build"],
            LakeTask::Test => &["test"],
        }
    }
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
pub use clew_core::cargorun::CargoTests;

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
            RunPlan::Doctest { item, file, .. } => {
                tr!("Doctest of {item}", item = doctest_item(item, file))
            }
            RunPlan::Script { file } | RunPlan::Lean { file, .. } => file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            // As it is typed: `lake build`, `lake test`.
            RunPlan::Lake { task, .. } => ["lake", task.args()[0]].join(" "),
            RunPlan::Remote { name, .. } => name.clone(),
        }
    }

    /// The one test it runs, by its function's name.
    pub(crate) fn test_name(&self) -> Option<&str> {
        match self {
            RunPlan::Cargo {
                test: Some(test), ..
            } => Some(&test.name),
            RunPlan::Pytest { name, .. } | RunPlan::Django { name, .. } => Some(name),
            RunPlan::Cargo { test: None, .. }
            | RunPlan::Script { .. }
            | RunPlan::Lean { .. }
            | RunPlan::Lake { .. }
            | RunPlan::Remote { .. }
            | RunPlan::Doctest { .. } => None,
        }
    }

    /// What the end of a run of one test says, by its exit `code`: that it
    /// passed or failed — or, for pytest, that it found no such test (5),
    /// could not load it (2: an import or collection error), or stopped
    /// with an error of its own (3, 4): the test never ran, and "failed"
    /// said it had. A group's run says the same of its tests. `None` for
    /// a run of anything but tests.
    pub(crate) fn verdict(&self, code: i64) -> Option<String> {
        if let RunPlan::Remote {
            name,
            tests: Some(group),
            ..
        } = self
        {
            return Some(match (group, code) {
                (true, 0) => tr!("Tests in {name} passed", name = name),
                (true, _) => tr!("Some tests in {name} failed", name = name),
                (false, 0) => tr!("Test {name} passed", name = name),
                (false, _) => tr!("Test {name} failed", name = name),
            });
        }
        if let RunPlan::Lake {
            project,
            task: LakeTask::Test,
            ..
        } = self
        {
            let name = file_name(project);
            return Some(match code {
                0 => tr!("Tests in {name} passed", name = name),
                _ => tr!("Some tests in {name} failed", name = name),
            });
        }
        if let RunPlan::Doctest { item, file, .. } = self {
            let item = doctest_item(item, file);
            return Some(match code {
                0 => tr!("Doctest of {item} passed", item = item),
                _ => tr!("Doctest of {item} failed", item = item),
            });
        }
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
                test.is_some() || matches!(target.kind, TargetKind::Test(_) | TargetKind::Bench(_))
            }
            RunPlan::Pytest { .. }
            | RunPlan::Django { .. }
            | RunPlan::Script { .. }
            | RunPlan::Lean { .. }
            | RunPlan::Lake { .. }
            | RunPlan::Remote { .. }
            | RunPlan::Doctest { .. } => false,
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
            RunPlan::Remote { .. } => (root.to_path_buf(), Vec::new(), root.to_path_buf(), None),
            RunPlan::Lake { project, task, .. } => (
                project.clone(),
                task.args().iter().map(|a| a.to_string()).collect(),
                project.clone(),
                None,
            ),
            RunPlan::Doctest { target, file, .. } => (
                file.clone(),
                Vec::new(),
                target.package_dir().to_path_buf(),
                None,
            ),
        }
    }

    /// The run's name as a path, its last part what its tab and the status
    /// line call it ([`Self::label`]): beside its package or its file, so
    /// runs of different packages' `main` are told apart.
    pub(crate) fn shown(&self) -> PathBuf {
        let beside = match self {
            RunPlan::Cargo { target, .. } | RunPlan::Doctest { target, .. } => {
                target.package_dir().to_path_buf()
            }
            RunPlan::Pytest { file, .. }
            | RunPlan::Django { file, .. }
            | RunPlan::Script { file }
            | RunPlan::Lean { file, .. } => {
                file.parent().map(Path::to_path_buf).unwrap_or_default()
            }
            RunPlan::Lake { project, .. } => project.clone(),
            RunPlan::Remote { .. } => PathBuf::new(),
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
        // A bench's own `main` (a `harness = false` bench) is what its ▶
        // runs, as `cargo bench` runs it: the package's program ran in its
        // place, or nothing did.
        TargetKind::Bin(_)
        | TargetKind::Example(_)
        | TargetKind::Test(_)
        | TargetKind::Bench(_) => Ok(RunPlan::Cargo {
            target: CargoTarget {
                module: Vec::new(),
                ..target
            },
            test: None,
        }),
        TargetKind::Lib => {
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
    let mut test = runnable_at(&found, line).filter(|r| {
        matches!(
            r.kind,
            RunnableKind::Test | RunnableKind::Tests | RunnableKind::Doctest
        )
    })?;
    match lang {
        // rustdoc runs a library's doc examples: a binary's are not tests.
        "rust" if test.kind == RunnableKind::Doctest => {
            let target =
                clew_core::cargorun::target_of(file).filter(|t| t.kind == TargetKind::Lib)?;
            Some(RunPlan::Doctest {
                target,
                file: file.to_path_buf(),
                line: test.line,
                item: test.name.clone(),
            })
        }
        "rust" => {
            let (target, runs, name) = clew_core::cargorun::tests_at(file, source, line)?;
            Some(RunPlan::Cargo {
                test: Some(CargoTest { runs, name }),
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

/// What a doc example documents, as its run is called: the item (`add`),
/// or a module's own example, its file (`lib.rs`).
fn doctest_item(item: &str, file: &Path) -> String {
    match item.is_empty() {
        true => file_name(file),
        false => item.to_string(),
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
