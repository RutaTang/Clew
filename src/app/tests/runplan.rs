//! Running what is in the code as its language's tools run it
//! (`app::runplan`): a Rust program built by Cargo when there is no launch
//! file, one test at a line — Rust's or Python's — run alone, and how a run
//! of a test ends; and the debugger reading a Rust panic.

use super::*;
use crate::app::runplan::{CargoTest, CargoTests, RunPlan, Tested};
use crate::app::state::RunTab;

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

const MANIFEST: &str =
    "[package]\nname = \"shapes\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n";

const LIB: &str = "\
pub fn area(r: f64) -> f64 {
    3.0 * r * r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(area(1.0), 3.0);
    }
}
";

/// Run on a Rust file with no launch file builds the program it is compiled
/// in with Cargo, the status line saying so and its tab named for it — not
/// "Create .clew/launch.json". A library's module runs its package's
/// program, said to be run for it; a library with no program says what to
/// run instead.
#[test]
fn run_on_a_rust_file_builds_its_program_with_cargo() {
    let mut app = scanned_app("runplan-cargo-run");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(&root, "Cargo.toml", MANIFEST);
    write(&root, "src/lib.rs", LIB);
    write(
        &root,
        "src/main.rs",
        "fn main() {\n    println!(\"{}\", shapes::area(2.0));\n}\n",
    );
    open_synchronously(&mut app, "src/main.rs", None);
    let _ = app.update(Message::Debug(DebugMsg::Run));
    let session = app.debug.session.as_ref().expect("a run");
    assert_eq!(session.program.file_name().unwrap(), "shapes");
    assert_eq!(app.status, "Building shapes with Cargo…");
    let Some(RunPlan::Cargo { target, test: None }) = &app.debug.plan else {
        panic!("{:?}", app.debug.plan);
    };
    assert_eq!(
        target.kind,
        clew_core::cargorun::TargetKind::Bin("shapes".into())
    );
    assert_eq!(app.debug.ran_for, None, "main.rs is what runs");
    let faces = app.run_tab_faces();
    assert_eq!(faces.len(), 1);
    assert_eq!(faces[0].name, "shapes");

    // A library's module: its package's program runs, said to be run for
    // the file being read.
    open_synchronously(&mut app, "src/lib.rs", None);
    let _ = app.update(Message::Debug(DebugMsg::Run));
    assert!(
        matches!(&app.debug.plan, Some(RunPlan::Cargo { target, .. })
            if target.kind == clew_core::cargorun::TargetKind::Bin("shapes".into())),
        "{:?}",
        app.debug.plan
    );
    assert_eq!(app.debug.ran_for.as_deref(), Some("lib.rs"));

    // A library with no program: nothing runs, and the status line says
    // what can — one of its tests.
    std::fs::remove_file(root.join("src/main.rs")).unwrap();
    let runs = app.run_tabs().len();
    let _ = app.update(Message::Debug(DebugMsg::Run));
    assert_eq!(app.run_tabs().len(), runs, "nothing started");
    assert!(app.status.contains("library"), "{}", app.status);
    assert!(app.status.contains("Run Test"), "{}", app.status);
}

/// Run Test on a Rust test runs it alone through Cargo, named by its path
/// in its test binary, its tab named for it; Debug Test the same with the
/// debugger. Where the cursor is in no test, it says so and runs nothing.
/// The run's end says the test's verdict; Run Again runs the same test,
/// whatever file is open by then.
#[test]
fn run_test_runs_the_rust_test_at_the_cursor() {
    let mut app = scanned_app("runplan-cargo-test");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(&root, "Cargo.toml", MANIFEST);
    write(&root, "src/lib.rs", LIB);
    open_synchronously(&mut app, "src/lib.rs", None);
    // In `area`, no test: the file's tests — of a library's root, its own
    // modules', not the crate's every test.
    assert_eq!(
        crate::app::runplan::test_plan_at("rust", &root.join("src/lib.rs"), LIB, 2).and_then(
            |plan| match plan {
                RunPlan::Cargo { test: Some(t), .. } => Some((t.runs, t.name)),
                _ => None,
            }
        ),
        Some((CargoTests::Under(vec!["tests".into()]), "lib.rs".into()))
    );
    // In `mod tests`, out of any test: the module's.
    assert_eq!(
        crate::app::runplan::test_plan_at("rust", &root.join("src/lib.rs"), LIB, 8).and_then(
            |plan| match plan {
                RunPlan::Cargo { test: Some(t), .. } => Some((t.runs, t.name)),
                _ => None,
            }
        ),
        Some((CargoTests::Under(vec!["tests".into()]), "tests".into()))
    );
    // A file with no test runs nothing, and says so.
    write(&root, "src/plain.rs", "pub fn f() {}\n");
    open_synchronously(&mut app, "src/plain.rs", None);
    let _ = app.update(Message::Debug(DebugMsg::RunTest {
        line: Some(1),
        no_debug: true,
    }));
    assert!(app.debug.session.is_none());
    assert!(app.status.starts_with("No test here"), "{}", app.status);
    open_synchronously(&mut app, "src/lib.rs", None);
    // In `adds`, at the cursor.
    app.active_viewer_mut().unwrap().caret = Some((11, 4));
    let _ = app.update(Message::Debug(DebugMsg::RunTest {
        line: None,
        no_debug: true,
    }));
    let Some(RunPlan::Cargo {
        test: Some(CargoTest { runs, name }),
        ..
    }) = &app.debug.plan
    else {
        panic!("{:?}", app.debug.plan);
    };
    assert_eq!(
        (runs, name.as_str()),
        (&CargoTests::One("tests::adds".into()), "adds")
    );
    assert!(app.debug.no_debug);
    assert_eq!(
        app.status,
        "Building the tests of shapes (library) with Cargo, to run adds…"
    );
    let tab = app.debug.tab;
    assert_eq!(app.run_tab_faces()[0].name, "adds");

    // Its end says how the test went.
    let _ = app.update(Message::Debug(DebugMsg::DapEvent {
        stamp: app.transport_stamp(),
        run: app.debug_run,
        event: dap::DapEvent::Exited { code: 101 },
    }));
    assert_eq!(app.status, "Test adds failed");
    let _ = app.update(Message::Debug(DebugMsg::DapEvent {
        stamp: app.transport_stamp(),
        run: app.debug_run,
        event: dap::DapEvent::Terminated,
    }));

    // Run Again: the same test, in its tab, though another file is open.
    open_synchronously(&mut app, "notes.txt", None);
    let _ = app.update(Message::Debug(DebugMsg::Again));
    assert_eq!(app.debug.tab, tab, "run again in its tab");
    assert!(matches!(
        &app.debug.plan,
        Some(RunPlan::Cargo { test: Some(t), .. }) if t.name == "adds"
    ));
    let _ = app.update(Message::Debug(DebugMsg::DapEvent {
        stamp: app.transport_stamp(),
        run: app.debug_run,
        event: dap::DapEvent::Exited { code: 0 },
    }));
    assert_eq!(app.status, "Test adds passed");

    // Debug Test: the same test, with the debugger, in a tab of its own.
    open_synchronously(&mut app, "src/lib.rs", None);
    let _ = app.update(Message::Debug(DebugMsg::RunTest {
        line: Some(12),
        no_debug: false,
    }));
    assert!(!app.debug.no_debug);
    assert_ne!(app.debug.tab, tab);
    assert!(
        app.debug
            .plan
            .as_ref()
            .is_some_and(|p| p.test_name() == Some("adds"))
    );
}

/// Run Test on a Python test runs it alone under pytest, by its node id —
/// its file, its class, its name — its tab named for the test; pytest
/// finding no such test is said as that, not as a failure.
#[test]
fn run_test_runs_the_python_test_at_the_line() {
    let mut app = scanned_app("runplan-pytest");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(
        &root,
        "tests/test_prices.py",
        "class TestPrices:\n    def test_total(self):\n        assert 1 + 1 == 2\n",
    );
    open_synchronously(&mut app, "tests/test_prices.py", None);
    let _ = app.update(Message::Debug(DebugMsg::RunTest {
        line: Some(3),
        no_debug: true,
    }));
    let Some(RunPlan::Pytest {
        file,
        node,
        name,
        tested,
    }) = app.debug.plan.clone()
    else {
        panic!("{:?}", app.debug.plan);
    };
    assert_eq!(tested, Tested::One);
    assert_eq!(file, root.join("tests/test_prices.py"));
    assert_eq!(
        node,
        format!(
            "{}::TestPrices::test_total",
            root.join("tests/test_prices.py").display()
        )
    );
    assert_eq!(name, "test_total");
    let session = app.debug.session.as_ref().unwrap();
    assert_eq!(session.program.file_name().unwrap(), "test_total");
    assert_eq!(session.args, [node]);
    assert_eq!(app.status, "Running test_total…");
    let _ = app.update(Message::Debug(DebugMsg::DapEvent {
        stamp: app.transport_stamp(),
        run: app.debug_run,
        event: dap::DapEvent::Exited { code: 5 },
    }));
    assert_eq!(app.status, "pytest found no test named test_total");
}

/// The code menu offers Run Test and Debug Test on a line in a test; out
/// of any test, its file's tests; in a file with none, nothing.
#[test]
fn the_code_menu_offers_the_test_at_its_line() {
    let mut app = scanned_app("runplan-menu");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(&root, "Cargo.toml", MANIFEST);
    write(&root, "src/lib.rs", LIB);
    open_synchronously(&mut app, "src/lib.rs", None);
    let pane = app.proj.active;
    assert_eq!(app.test_at(pane, 12), Some(crate::TestAt::One));
    assert_eq!(
        app.test_at(pane, 2),
        Some(crate::TestAt::Group("lib.rs".into())),
        "in `area`"
    );
    open_synchronously(&mut app, "notes.txt", None);
    assert!(app.test_at(app.proj.active, 1).is_none());
}

/// A Rust program that panicked stops in its panic machinery, whose files
/// are not on this disk: the stop is shown at the reader's code that
/// panicked — its line, its variables — and said as a panic, not as a
/// breakpoint.
#[test]
fn a_rust_panic_is_shown_where_the_program_panicked() {
    let mut app = scanned_app("runplan-panic");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(
        &root,
        "src/main.rs",
        "fn main() {\n    panic!(\"no\");\n}\n",
    );
    std::fs::write(root.join("app.py"), "print('hi')\n").unwrap();
    open_synchronously(&mut app, "app.py", None);
    let _ = app.start_debug();
    let run = app.debug_run;
    let frame = |id: i64, name: &str, path: &str, line: usize| crate::dap::StackFrame {
        id,
        name: name.into(),
        path: Some(PathBuf::from(path)),
        line,
        column: 1,
    };
    let main = root.join("src/main.rs");
    let _ = app.update(Message::Debug(DebugMsg::DapEvent {
        stamp: app.transport_stamp(),
        run,
        event: dap::DapEvent::Stopped(dap::proto::Stopped {
            reason: "breakpoint".into(),
            thread_id: Some(1),
            description: None,
            text: None,
            all_threads: true,
        }),
    }));
    let _ = app.update(Message::Debug(DebugMsg::DapStopInspected {
        stamp: app.transport_stamp(),
        run,
        stop: app.debug_stop,
        record: true,
        frames: vec![
            frame(
                1,
                "__rustc::rust_panic",
                "/rustc/abc/library/std/src/panicking.rs",
                886,
            ),
            frame(
                2,
                "core::panicking::panic_fmt",
                "/rustc/abc/library/core/src/panicking.rs",
                80,
            ),
            frame(3, "shapes::main", &main.to_string_lossy(), 2),
        ],
        scopes: Vec::new(),
    }));
    let session = app.debug.session.as_ref().unwrap();
    assert_eq!(session.current, Some((main.clone(), 2)));
    // Said so — the status line then says the file it opens is loading.
    assert_eq!(app.debug.said.as_deref(), Some("Paused: panic"));
    assert_eq!(app.debug.trace.last().unwrap().reason, "panic");
    assert!(app.run_tabs().contains(&RunTab::Real(app.debug.tab)));
}

/// Cargo unable to build the program: the run ends with its tab kept,
/// Cargo's errors in it, marked failed and said "build failed"; the status
/// line says where the errors are. Run Again builds it again, the mark
/// gone. (Built with this machine's Cargo; skipped without one.)
#[test]
fn a_failed_build_keeps_its_tab_with_cargos_errors() {
    if clew_core::cargorun::find_cargo().is_none() {
        eprintln!("no cargo here: skipped");
        return;
    }
    let mut app = scanned_app("runplan-build-fails");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(&root, "Cargo.toml", MANIFEST);
    write(
        &root,
        "src/main.rs",
        "fn main() {\n    let n: u32 = \"three\";\n}\n",
    );
    open_synchronously(&mut app, "src/main.rs", None);
    let task = app.update(Message::Debug(DebugMsg::Run));
    let tab = app.debug.tab;
    for message in run_task(task) {
        let _ = app.update(message);
    }
    let session = app.debug.session.as_ref().expect("its tab stays");
    assert_eq!(session.status, DebugStatus::Terminated);
    let output: String = session.output.iter().map(|(_, t)| t.as_str()).collect();
    assert!(output.contains("mismatched types"), "{output}");
    assert_eq!(
        app.status,
        "Cargo could not build shapes. Its errors are in the run's output."
    );
    assert!(app.debug.build_failed.contains(&tab));
    let face = app
        .run_tab_faces()
        .into_iter()
        .find(|f| f.tab == RunTab::Real(tab));
    assert_eq!(face.unwrap().state, crate::app::runs::TabState::Failed);
    let _ = app.update(Message::Debug(DebugMsg::Again));
    assert_eq!(app.debug.tab, tab);
    assert!(!app.debug.build_failed.contains(&tab), "building again");
    assert_eq!(app.status, "Building shapes with Cargo…");
    let _ = std::fs::remove_dir_all(root.join("target"));
}

/// A Lean file's `main` runs as Lean runs it — `lake env lean --run`, in
/// its Lake project — its output and its exit in its run's tab; Run on a
/// Lean file told the reader to write a launch file no debugger could use.
/// One with no `main`, or Debug, says what there is instead.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_lean_files_main_runs_through_lake() {
    use std::os::unix::fs::PermissionsExt;
    let mut app = scanned_app("runplan-lean");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(&root, "lakefile.lean", "import Lake\n");
    write(
        &root,
        "Main.lean",
        "def main : IO Unit := IO.println \"hi\"\n",
    );
    write(&root, "Proofs.lean", "theorem t : 1 = 1 := rfl\n");
    scan_synchronously(&mut app, root.clone());
    open_synchronously(&mut app, "Proofs.lean", None);
    let _ = app.update(Message::Debug(DebugMsg::Run));
    assert!(app.debug.session.is_none(), "nothing runs");
    assert!(app.status.contains("no main"), "{}", app.status);
    open_synchronously(&mut app, "Main.lean", None);
    let _ = app.update(Message::Debug(DebugMsg::Start));
    assert!(app.debug.session.is_none(), "no debugger to start");
    assert!(app.status.contains("no debugger"), "{}", app.status);

    // Run, with a lake standing in for Lean's.
    let tools = clew_core::testutil::TempDir::new("runplan-lean-lake");
    let lake = tools.path().join("lake");
    std::fs::write(
        &lake,
        "#!/bin/sh\necho \"hi from $4\"\necho careful >&2\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&lake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let file = root.join("Main.lean");
    let task = app.start_session(
        true,
        Some(RunPlan::Lean {
            file: file.clone(),
            project: root.clone(),
            lake,
        }),
    );
    assert_eq!(app.status, "Running Main.lean with Lean…");
    assert_eq!(app.run_tab_faces()[0].name, "Main.lean");
    for msg in run_task_async(task).await {
        let _ = app.update(msg);
    }
    let session = app.debug.session.as_ref().expect("its run");
    assert_eq!(session.status, crate::DebugStatus::Terminated);
    assert_eq!(session.exit_code, Some(0));
    let said: Vec<&str> = session.output.iter().map(|(_, t)| t.as_str()).collect();
    assert!(
        said.iter()
            .any(|t| t.contains(&format!("hi from {}", file.display()))),
        "{said:?}"
    );
    assert!(said.iter().any(|t| t.contains("careful")), "{said:?}");
    // Ended: the status line no longer says it runs.
    assert!(!app.status.contains("Running"), "{}", app.status);
}

/// A test method of a Django project — a `manage.py` above it, Django
/// used in its file — runs by Django's runner, by its dotted label, in the
/// project's folder: under pytest alone Django's settings were never set,
/// and it failed before it ran. Where pytest is set up for Django, or the
/// test is a bare function, pytest runs it as before.
#[test]
fn a_django_projects_test_runs_by_djangos_runner() {
    let root = test_dir("runplan-django");
    let _ = std::fs::remove_dir_all(&root);
    write(&root, "manage.py", "import django\n");
    let tests = "from django.test import TestCase\n\n\nclass CartTests(TestCase):\n    def test_total(self):\n        self.assertEqual(1, 1)\n\n\ndef test_loose():\n    assert True\n";
    write(&root, "shop/tests/test_cart.py", tests);
    let file = root.join("shop/tests/test_cart.py");
    let plan = crate::app::runplan::test_plan_at("python", &file, tests, 6).expect("a test");
    let RunPlan::Django {
        manage,
        label,
        name,
        ..
    } = &plan
    else {
        panic!("Django's runner: {plan:?}");
    };
    assert_eq!(manage, &root.join("manage.py"));
    assert_eq!(label, "shop.tests.test_cart.CartTests.test_total");
    assert_eq!(name, "test_total");
    let (program, args, cwd, lang) = plan.launch(&root);
    assert_eq!(program, root.join("manage.py"));
    assert_eq!(args, ["test", label.as_str(), "--noinput"]);
    assert_eq!(cwd, root);
    assert_eq!(lang, Some(crate::dap::Lang::Python));
    assert_eq!(plan.verdict(0).unwrap(), "Test test_total passed");
    assert_eq!(plan.verdict(1).unwrap(), "Test test_total failed");
    // A bare function is no Django test: pytest's.
    let loose = crate::app::runplan::test_plan_at("python", &file, tests, 10).expect("a test");
    assert!(matches!(loose, RunPlan::Pytest { .. }), "{loose:?}");
    // pytest set up for Django runs them all.
    write(
        &root,
        "pytest.ini",
        "[pytest]\nDJANGO_SETTINGS_MODULE = shop.settings\n",
    );
    let plan = crate::app::runplan::test_plan_at("python", &file, tests, 6).expect("a test");
    assert!(matches!(plan, RunPlan::Pytest { .. }), "{plan:?}");
}

/// From a line of a Python test file, Run Test runs what the line is in:
/// a test, one case of a parametrized test, a test class's tests, or —
/// anywhere else — the file's. The code menu names each, and each run
/// says how its tests ended.
#[test]
fn run_test_runs_a_class_a_file_or_a_case_from_their_lines() {
    let mut app = scanned_app("runplan-groups");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let source = "\
import pytest


def helper():
    return 1


class TestCart:
    total = 0

    def test_total(self):
        assert helper() == 1


@pytest.mark.parametrize(\"a, b\", [
    (1, 2),
    (3, 4),
])
def test_add(a, b):
    assert a < b
";
    write(&root, "tests/test_cart.py", source);
    open_synchronously(&mut app, "tests/test_cart.py", None);
    let pane = app.proj.active;
    let file = root.join("tests/test_cart.py");
    let at = |line| crate::app::runplan::test_plan_at("python", &file, source, line).unwrap();
    // What the code menu offers at each line.
    assert_eq!(app.test_at(pane, 12), Some(crate::TestAt::One));
    assert_eq!(
        app.test_at(pane, 9),
        Some(crate::TestAt::Group("TestCart".into()))
    );
    assert_eq!(
        app.test_at(pane, 5),
        Some(crate::TestAt::Group("test_cart.py".into()))
    );
    assert_eq!(
        app.test_at(pane, 17),
        Some(crate::TestAt::Case("test_add[3-4]".into()))
    );
    // A class's tests, by the class's node id.
    let class = at(10);
    let RunPlan::Pytest {
        node, name, tested, ..
    } = &class
    else {
        panic!("{class:?}");
    };
    assert_eq!(node, &format!("{}::TestCart", file.display()));
    assert_eq!((name.as_str(), *tested), ("TestCart", Tested::Group));
    assert_eq!(class.verdict(0).unwrap(), "Tests in TestCart passed");
    assert_eq!(class.verdict(1).unwrap(), "Some tests in TestCart failed");
    assert_eq!(
        class.verdict(5).unwrap(),
        "pytest found no tests in TestCart"
    );
    // The file's, from a line in none of them.
    let whole = at(5);
    let RunPlan::Pytest { node, name, .. } = &whole else {
        panic!("{whole:?}");
    };
    assert_eq!(node, &file.display().to_string());
    assert_eq!(name, "test_cart.py");
    // One case, alone; the whole test from its `def`.
    let RunPlan::Pytest { node, tested, .. } = at(17) else {
        panic!();
    };
    assert_eq!(node, format!("{}::test_add[3-4]", file.display()));
    assert_eq!(tested, Tested::One);
    let RunPlan::Pytest { node, .. } = at(19) else {
        panic!();
    };
    assert_eq!(node, format!("{}::test_add", file.display()));
    // A project whose hook names cases its own way: the whole test.
    write(
        &root,
        "conftest.py",
        "def pytest_make_parametrize_id(config, val, argname):\n    return None\n",
    );
    let RunPlan::Pytest { node, name, .. } = at(17) else {
        panic!();
    };
    assert_eq!(node, format!("{}::test_add", file.display()));
    assert_eq!(name, "test_add");
    assert_eq!(app.test_at(pane, 17), Some(crate::TestAt::One));
    // Run from the menu's line, the run is named for what it runs.
    let _ = app.update(Message::Debug(DebugMsg::RunTest {
        line: Some(9),
        no_debug: true,
    }));
    assert_eq!(app.status, "Running TestCart…");
}

/// In a Django project, a test class and a test module run by Django's
/// runner too, by their dotted labels; a module with no test class has
/// nothing Django's runner runs, and pytest runs it.
#[test]
fn a_django_class_or_module_runs_by_its_label() {
    let root = test_dir("runplan-django-groups");
    let _ = std::fs::remove_dir_all(&root);
    write(&root, "manage.py", "import django\n");
    let tests = "from django.test import TestCase\n\n\nclass CartTests(TestCase):\n    def test_total(self):\n        self.assertEqual(1, 1)\n\n\ndef helper():\n    pass\n";
    write(&root, "shop/tests.py", tests);
    let file = root.join("shop/tests.py");
    let label_at = |line| match crate::app::runplan::test_plan_at("python", &file, tests, line) {
        Some(RunPlan::Django {
            label,
            name,
            tested,
            ..
        }) => (label, name, tested),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        label_at(4),
        (
            "shop.tests.CartTests".into(),
            "CartTests".into(),
            Tested::Group
        )
    );
    assert_eq!(
        label_at(10),
        ("shop.tests".into(), "tests.py".into(), Tested::Group)
    );
    let loose = "import django\n\n\ndef test_loose():\n    assert True\n";
    write(&root, "shop/test_loose.py", loose);
    let file = root.join("shop/test_loose.py");
    let plan = crate::app::runplan::test_plan_at("python", &file, loose, 1).expect("the file");
    assert!(matches!(plan, RunPlan::Pytest { .. }), "{plan:?}");
}

/// A Lean file's `main` has its ▶, on its name: Run alone in its menu —
/// Lean has no debugger — and run as Run runs the file, through Lake. One
/// in a namespace is none.
#[test]
fn a_lean_main_has_a_play_mark_that_runs_it() {
    let mut app = scanned_app("runplan-lean-main");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(
        &root,
        "Main.lean",
        "def greet := \"hi\"\n\ndef main : IO Unit :=\n  IO.println greet\n",
    );
    write(
        &root,
        "Other.lean",
        "namespace Other\ndef main : IO Unit := pure ()\nend Other\n",
    );
    open_synchronously(&mut app, "Other.lean", None);
    let pane = app.proj.active;
    let v = app.proj.panes[pane].as_ref().unwrap();
    assert!(app.run_marks(v).is_empty(), "`Other.main` is no program");
    open_synchronously(&mut app, "Main.lean", None);
    let v = app.proj.panes[pane].as_ref().unwrap();
    assert_eq!(app.run_marks(v).into_iter().collect::<Vec<_>>(), [3]);
    let _ = app.update(Message::Hover(HoverMsg::RunMarkClicked {
        pane,
        line: 3,
        x: 4.0,
        y: 40.0,
    }));
    let menu = app.proj.context_menu.clone().expect("the ▶'s menu");
    assert_eq!(
        menu.gutter,
        Some(crate::GutterRun {
            main: Some("Main.lean".into()),
            debug: false
        })
    );
    assert_eq!(menu.test, None);
    // Run: through Lake — not found here, which it says — the menu gone.
    let _no_lean = clew_core::testutil::EnvVars::new().set("ELAN_HOME", root.join("no-elan"));
    let _ = app.update(Message::Debug(DebugMsg::RunMain {
        line: 3,
        no_debug: true,
    }));
    assert!(app.status.contains("lake"), "{}", app.status);
    assert!(app.proj.context_menu.is_none());
}

/// Lean: Build Project and Lean: Run Project Tests run Lake in the Lake
/// project of the Lean file read — `lake build`, `lake test` — in the run
/// panel; without Lean, it is said how to get it.
#[cfg(unix)]
#[test]
fn lake_builds_and_tests_the_lean_files_project() {
    use crate::app::runplan::LakeTask;
    use std::os::unix::fs::PermissionsExt;
    let mut app = scanned_app("runplan-lake-tasks");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(&root, "proofs/lakefile.toml", "name = \"proofs\"\n");
    write(&root, "proofs/Proofs.lean", "theorem t : 1 = 1 := rfl\n");
    open_synchronously(&mut app, "proofs/Proofs.lean", None);
    {
        let _no_lean = clew_core::testutil::EnvVars::new().set("ELAN_HOME", root.join("no-elan"));
        let _ = app.update(Message::Debug(DebugMsg::RunLake(LakeTask::Build)));
        assert!(app.status.contains("lake"), "{}", app.status);
        assert!(app.debug.plan.is_none());
    }
    let elan = root.join("elan");
    std::fs::create_dir_all(elan.join("bin")).unwrap();
    let lake = elan.join("bin/lake");
    std::fs::write(&lake, "#!/bin/sh\necho \"lake $*\"\n").unwrap();
    std::fs::set_permissions(&lake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _elan = clew_core::testutil::EnvVars::new().set("ELAN_HOME", &elan);
    for task in [LakeTask::Build, LakeTask::Test] {
        let _ = app.update(Message::Debug(DebugMsg::RunLake(task)));
        assert!(
            matches!(&app.debug.plan, Some(RunPlan::Lake { project, task: t, .. })
                if *project == root.join("proofs") && *t == task),
            "{:?}",
            app.debug.plan
        );
    }
    let plan = app.debug.plan.clone().unwrap();
    assert_eq!(plan.label(), "lake test");
    assert_eq!(plan.verdict(0).as_deref(), Some("Tests in proofs passed"));
}

/// The gutter's ▶: on a Rust file's tests, its test module and its `main`,
/// and a Python file's test class, tests and `__main__` block — not on a
/// parametrized test's cases. A click opens the short code menu there:
/// Run and Debug of what starts on the line, and the breakpoint the ▶
/// stands in for. Its rows run that, whatever a launch file names.
#[test]
fn a_gutter_play_mark_runs_or_debugs_what_starts_on_its_line() {
    let mut app = scanned_app("runplan-gutter-marks");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(&root, "Cargo.toml", MANIFEST);
    write(&root, "src/lib.rs", LIB);
    write(
        &root,
        "src/main.rs",
        "fn main() {\n    println!(\"{}\", shapes::area(2.0));\n}\n",
    );
    // A launch file naming another program: the ▶ runs this one.
    write(
        &root,
        ".clew/launch.json",
        r#"{"configurations": [{"type": "python", "program": "other.py"}]}"#,
    );
    write(
        &root,
        "app.py",
        "import pytest\n\nclass TestCart:\n    def test_total(self):\n        pass\n\n\
         @pytest.mark.parametrize('n', [1, 2])\ndef test_n(n):\n    pass\n\n\
         if __name__ == '__main__':\n    print('hi')\n",
    );

    open_synchronously(&mut app, "src/lib.rs", None);
    let pane = app.proj.active;
    let marks = |app: &App| {
        let v = app.proj.panes[pane].as_ref().unwrap();
        let mut lines: Vec<usize> = app.run_marks(v).into_iter().collect();
        lines.sort();
        lines
    };
    assert_eq!(marks(&app), [6, 10], "`mod tests` and `adds`");

    // A click on `adds`'s ▶: its menu, short.
    let _ = app.update(Message::Hover(HoverMsg::RunMarkClicked {
        pane,
        line: 10,
        x: 4.0,
        y: 200.0,
    }));
    let menu = app.proj.context_menu.clone().expect("the ▶'s menu");
    assert_eq!(menu.test, Some(crate::TestAt::One));
    assert_eq!(
        menu.gutter,
        Some(crate::GutterRun {
            main: None,
            debug: true
        })
    );
    assert_eq!(menu.line, 9, "0-based, as the code menu's");
    // On the module: its tests.
    let _ = app.update(Message::Hover(HoverMsg::ContextMenuClosed));
    let _ = app.update(Message::Hover(HoverMsg::RunMarkClicked {
        pane,
        line: 6,
        x: 4.0,
        y: 120.0,
    }));
    assert_eq!(
        app.proj.context_menu.as_ref().and_then(|m| m.test.clone()),
        Some(crate::TestAt::Group("tests".into()))
    );
    let _ = app.update(Message::Hover(HoverMsg::ContextMenuClosed));

    // `main`: named for its program, and run as Cargo builds it.
    open_synchronously(&mut app, "src/main.rs", None);
    let pane = app.proj.active;
    let v = app.proj.panes[pane].as_ref().unwrap();
    assert_eq!(app.run_marks(v).into_iter().collect::<Vec<_>>(), [1]);
    let _ = app.update(Message::Hover(HoverMsg::RunMarkClicked {
        pane,
        line: 1,
        x: 4.0,
        y: 10.0,
    }));
    let menu = app.proj.context_menu.clone().expect("the ▶'s menu");
    assert_eq!(menu.test, None);
    assert_eq!(
        menu.gutter,
        Some(crate::GutterRun {
            main: Some("shapes".into()),
            debug: true
        })
    );
    let _ = app.update(Message::Debug(DebugMsg::RunMain {
        line: 1,
        no_debug: true,
    }));
    assert!(
        matches!(&app.debug.plan, Some(RunPlan::Cargo { target, test: None })
            if target.kind == clew_core::cargorun::TargetKind::Bin("shapes".into())),
        "{:?}",
        app.debug.plan
    );

    // Python: the class, its test and the parametrized test, each once
    // (no ▶ on its cases' lines), and the `__main__` block.
    open_synchronously(&mut app, "app.py", None);
    let pane = app.proj.active;
    let v = app.proj.panes[pane].as_ref().unwrap();
    let mut lines: Vec<usize> = app.run_marks(v).into_iter().collect();
    lines.sort();
    assert_eq!(lines, [3, 4, 8, 11]);
    assert_eq!(app.main_at(pane, 11).as_deref(), Some("app.py"));
    let runs = app.run_tabs().len();
    let _ = app.update(Message::Debug(DebugMsg::RunMain {
        line: 11,
        no_debug: false,
    }));
    assert_eq!(app.run_tabs().len(), runs + 1, "a run of its own");
    assert_eq!(
        app.debug.plan,
        Some(RunPlan::Script {
            file: root.join("app.py")
        }),
        "the file, not the launch file's program"
    );
    // Where nothing starts, nothing runs.
    let _ = app.update(Message::Debug(DebugMsg::RunMain {
        line: 2,
        no_debug: true,
    }));
    assert_eq!(app.status, "No program starts here.");
}

/// A doc comment's example is run alone, as rustdoc runs it: marked ▶ on
/// the line its code block opens, offered to run and not to debug, and
/// Debug Test on it runs it, said to run without the debugger. A binary's
/// examples are no doctests.
#[test]
fn a_doc_example_runs_as_a_doctest() {
    let mut app = scanned_app("runplan-doctest");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(&root, "Cargo.toml", MANIFEST);
    let lib = "/// Twice.\n///\n/// ```\n/// assert_eq!(shapes::twice(2), 4);\n/// ```\npub fn twice(x: i32) -> i32 {\n    x * 2\n}\n";
    write(&root, "src/lib.rs", lib);
    write(
        &root,
        "src/main.rs",
        "/// ```\n/// run();\n/// ```\nfn run() {}\nfn main() { run() }\n",
    );
    open_synchronously(&mut app, "src/lib.rs", None);
    let pane = app.proj.active;
    let v = app.proj.panes[pane].as_ref().unwrap();
    assert_eq!(app.run_marks(v).into_iter().collect::<Vec<_>>(), [3]);
    let _ = app.update(Message::Hover(HoverMsg::RunMarkClicked {
        pane,
        line: 3,
        x: 4.0,
        y: 60.0,
    }));
    let menu = app.proj.context_menu.clone().expect("the ▶'s menu");
    assert_eq!(
        menu.test,
        Some(crate::TestAt::Doctest("Doctest of twice".into()))
    );
    let _ = app.update(Message::Hover(HoverMsg::ContextMenuClosed));
    assert_eq!(
        crate::app::runplan::test_plan_at("rust", &root.join("src/lib.rs"), lib, 4),
        Some(RunPlan::Doctest {
            target: clew_core::cargorun::target_of(&root.join("src/lib.rs")).unwrap(),
            file: root.join("src/lib.rs"),
            line: 3,
            item: "twice".into(),
        })
    );
    // Debug Test in it: run, never debugged, and said so.
    let _ = app.update(Message::Debug(DebugMsg::RunTest {
        line: Some(4),
        no_debug: false,
    }));
    assert!(
        matches!(app.debug.plan, Some(RunPlan::Doctest { .. })),
        "{:?}",
        app.debug.plan
    );
    assert!(app.debug.no_debug);
    assert!(
        app.status.starts_with(
            "A doc example runs without the debugger: Running Doctest of twice with Cargo"
        ),
        "{}",
        app.status
    );
    let plan = app.debug.plan.clone().unwrap();
    assert_eq!(plan.verdict(0).as_deref(), Some("Doctest of twice passed"));
    assert_eq!(
        plan.verdict(101).as_deref(),
        Some("Doctest of twice failed")
    );
    // A binary's example: no doctest.
    let main = "/// ```\n/// run();\n/// ```\nfn run() {}\nfn main() { run() }\n";
    assert_eq!(
        crate::app::runplan::test_plan_at("rust", &root.join("src/main.rs"), main, 2),
        None
    );
}

/// A bench's own `main` (`harness = false`) is what its ▶ runs, built as
/// `cargo bench` builds it.
#[test]
fn a_benchs_main_runs_the_bench() {
    let mut app = scanned_app("runplan-bench");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(
        &root,
        "Cargo.toml",
        &format!("{MANIFEST}\n[[bench]]\nname = \"speed\"\nharness = false\n"),
    );
    write(&root, "src/lib.rs", "pub fn f() {}\n");
    write(&root, "benches/speed.rs", "fn main() {}\n");
    let plan = crate::app::runplan::cargo_plan_for(&root.join("benches/speed.rs")).unwrap();
    assert!(
        matches!(&plan, RunPlan::Cargo { target, test: None }
            if target.kind == clew_core::cargorun::TargetKind::Bench("speed".into())),
        "{plan:?}"
    );
    assert!(plan.builds_tests(), "built as Cargo builds a bench");
    open_synchronously(&mut app, "benches/speed.rs", None);
    assert_eq!(
        app.main_at(app.proj.active, 1).as_deref(),
        Some("the speed benchmark")
    );
}

/// Rust's sources missing from the project's toolchain are offered once
/// rust-analyzer starts, added on request, and rust-analyzer started again
/// to read them; a failure keeps rustup's word and the way to try again.
#[test]
fn missing_rust_sources_are_added_on_request() {
    use crate::app::RustSrc;
    let mut app = scanned_app("runplan-rust-src");
    app.proj.rust_src = RustSrc::Missing(Some("1.80.0".into()));
    let _ = app.update(Message::Lsp(crate::LspMsg::AddRustSrc));
    assert_eq!(app.proj.rust_src, RustSrc::Adding(Some("1.80.0".into())));
    // Asked again while it adds them: nothing more.
    let _ = app.update(Message::Lsp(crate::LspMsg::AddRustSrc));
    assert_eq!(app.proj.rust_src, RustSrc::Adding(Some("1.80.0".into())));
    let stamp = app.stamp();
    let _ = app.update(Message::Lsp(crate::LspMsg::RustSrcAdded {
        stamp: stamp.clone(),
        result: Err("no network".into()),
    }));
    assert_eq!(
        app.proj.rust_src,
        RustSrc::Failed {
            toolchain: Some("1.80.0".into()),
            error: "no network".into()
        }
    );
    let _ = app.update(Message::Lsp(crate::LspMsg::AddRustSrc));
    let _ = app.update(Message::Lsp(crate::LspMsg::RustSrcAdded {
        stamp,
        result: Ok(()),
    }));
    assert_eq!(app.proj.rust_src, RustSrc::Present);
    assert!(
        app.status.starts_with("Rust's sources were added."),
        "{}",
        app.status
    );
    // Looked at once a project.
    let stamp = app.stamp();
    let _ = app.update(Message::Lsp(crate::LspMsg::RustSrcFound {
        stamp,
        found: Some(Err(None)),
    }));
    assert_eq!(app.proj.rust_src, RustSrc::Missing(None));
}

/// Run Test on a JavaScript or TypeScript test runs it by the project's own
/// test runner where the project is: the test at the line by its title, its
/// suite's tests from the suite's line, and the file's from anywhere else;
/// its end says how it went. Debug Test runs it too, saying it runs without
/// the debugger. The gutter marks each, and the code menu names it.
#[test]
fn run_test_runs_the_javascript_test_at_the_line() {
    let mut app = scanned_app("runplan-js-test");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(
        &root,
        "package.json",
        r#"{"devDependencies": {"vitest": "^3"}}"#,
    );
    write(
        &root,
        "test/cart.test.ts",
        "import { describe, it } from 'vitest'\n\ndescribe('cart', () => {\n  it('adds', () => {})\n  it('drops', () => {})\n})\n",
    );
    open_synchronously(&mut app, "test/cart.test.ts", None);
    let run = |app: &mut App, line: usize, no_debug: bool| {
        let _ = app.update(Message::Debug(DebugMsg::RunTest {
            line: Some(line),
            no_debug,
        }));
        app.debug.plan.clone()
    };
    let task = |line| clew_protocol::ProjectTask::JsTest {
        file: "test/cart.test.ts".into(),
        line,
    };
    assert_eq!(
        run(&mut app, 4, true),
        Some(RunPlan::Remote {
            task: task(4),
            name: "adds".into(),
            tests: Some(false),
        })
    );
    assert!(app.debug.no_debug);
    assert_eq!(app.status, "Running adds…", "here, not on a remote host");
    let _ = app.update(Message::Debug(DebugMsg::DapEvent {
        stamp: app.transport_stamp(),
        run: app.debug_run,
        event: dap::DapEvent::Exited { code: 0 },
    }));
    assert_eq!(app.status, "Test adds passed");
    assert_eq!(
        run(&mut app, 3, true),
        Some(RunPlan::Remote {
            task: task(3),
            name: "cart".into(),
            tests: Some(true),
        })
    );
    let _ = app.update(Message::Debug(DebugMsg::DapEvent {
        stamp: app.transport_stamp(),
        run: app.debug_run,
        event: dap::DapEvent::Exited { code: 1 },
    }));
    assert_eq!(app.status, "Some tests in cart failed");
    assert_eq!(
        run(&mut app, 1, true),
        Some(RunPlan::Remote {
            task: task(1),
            name: "cart.test.ts".into(),
            tests: Some(true),
        })
    );
    // Debug Test: run, and said to be without the debugger.
    let _ = run(&mut app, 5, false);
    assert!(app.debug.no_debug);
    assert!(
        app.status
            .starts_with("JavaScript and TypeScript tests run without the debugger"),
        "{}",
        app.status
    );
    // The gutter marks the suite and its tests; the code menu names what
    // runs at a line.
    let marks = app.run_marks(app.active_viewer().unwrap());
    assert_eq!(marks, [3, 4, 5].into_iter().collect());
    let pane = app.proj.active;
    assert_eq!(app.test_at(pane, 4), Some(crate::TestAt::One));
    assert_eq!(
        app.test_at(pane, 3),
        Some(crate::TestAt::Group("cart".into()))
    );
    // Not a test: said so.
    write(&root, "src/util.ts", "export const one = 1\n");
    open_synchronously(&mut app, "src/util.ts", None);
    let before = app.debug.plan.clone();
    let _ = run(&mut app, 1, true);
    assert_eq!(app.debug.plan, before);
    assert_eq!(
        app.status,
        "No test here. Put the cursor in a test function to run it."
    );
}
