//! Running what is in the code as its language's tools run it
//! (`app::runplan`): a Rust program built by Cargo when there is no launch
//! file, one test at a line — Rust's or Python's — run alone, and how a run
//! of a test ends; and the debugger reading a Rust panic.

use super::*;
use crate::app::runplan::{CargoTest, RunPlan};
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
    // In `area`, no test: nothing runs.
    let _ = app.update(Message::Debug(DebugMsg::RunTest {
        line: Some(2),
        no_debug: true,
    }));
    assert!(app.debug.session.is_none());
    assert!(app.status.starts_with("No test here"), "{}", app.status);
    // In `adds`, at the cursor.
    app.active_viewer_mut().unwrap().caret = Some((11, 4));
    let _ = app.update(Message::Debug(DebugMsg::RunTest {
        line: None,
        no_debug: true,
    }));
    let Some(RunPlan::Cargo {
        test: Some(CargoTest { path, name }),
        ..
    }) = &app.debug.plan
    else {
        panic!("{:?}", app.debug.plan);
    };
    assert_eq!((path.as_str(), name.as_str()), ("tests::adds", "adds"));
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
    let Some(RunPlan::Pytest { file, node, name }) = app.debug.plan.clone() else {
        panic!("{:?}", app.debug.plan);
    };
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

/// The code menu offers Run Test and Debug Test on a line in a test, and
/// only there.
#[test]
fn the_code_menu_offers_the_test_at_its_line() {
    let mut app = scanned_app("runplan-menu");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    write(&root, "Cargo.toml", MANIFEST);
    write(&root, "src/lib.rs", LIB);
    open_synchronously(&mut app, "src/lib.rs", None);
    let pane = app.proj.active;
    assert!(app.test_at(pane, 12).is_some_and(|name| name == "adds"));
    assert!(app.test_at(pane, 2).is_none(), "in `area`");
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
