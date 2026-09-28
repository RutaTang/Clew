//! Helpers the unit tests share: scratch directories that clean up after
//! themselves, and running one test in a child process with its own
//! environment.

use std::path::Path;

/// A directory unique to this test process and call — never a fixed name two
/// runs (or two tests) could share — removed with everything in it on drop,
/// panicking tests included. The workspace's one implementation, shared with
/// clew-core's and the GUI's tests.
pub(crate) use clew_core::testutil::TempDir as Scratch;

/// Set in a child started by [`child_output`], for the child-side test.
const CHILD_MARK: &str = "CLEW_SERVER_TEST_CHILD";

/// Whether this process is a child started by [`child_output`]. A child-side
/// test returns at once otherwise, so running the ignored tests directly
/// (`--include-ignored`) passes without doing anything.
pub(crate) fn in_child() -> bool {
    std::env::var_os(CHILD_MARK).is_some()
}

/// Run the ignored test `name` (its path inside this crate, e.g.
/// `lsp_gate::tests::child_x`) alone in a child process of this test binary,
/// with `env` set there, and return how that process ended.
///
/// For tests that need a process-wide setting — `CLEW_DATA_DIR` — without
/// `set_var`. The harness runs tests on parallel threads, and a variable
/// changed under them races every other test that reads the environment:
/// through `std::env` (a logical race — a test resolving the wrong data
/// directory), and through libc underneath DNS lookups or the system trust
/// store (undefined behaviour, which no lock of ours covers). The child has
/// the setting from birth and runs that one test by itself.
pub(crate) fn child_output(name: &str, env: &[(&str, &Path)]) -> std::process::Output {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            name,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_MARK, "1");
    for (key, value) in env {
        child.env(key, value);
    }
    child.output().expect("the test binary runs again")
}

/// [`child_output`], failing unless the child ran `name` and it passed.
pub(crate) fn run_in_child(name: &str, env: &[(&str, &Path)]) {
    let out = child_output(name, env);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "{name} failed in its child process ({}):\n{stdout}\n{stderr}",
        out.status
    );
}

/// Put a `go` in `bin` that records its pid in `bin/started`, then waits
/// for a minute (by absolute path: `bin` is the whole PATH).
pub(crate) fn fake_slow_go(bin: &Path) {
    let go = bin.join("go");
    std::fs::write(
        &go,
        "#!/bin/sh\necho $$ > \"${0%/*}/started.tmp\"\n\
         /bin/mv \"${0%/*}/started.tmp\" \"${0%/*}/started\"\n\
         exec /bin/sleep 60\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&go, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The pid [`fake_slow_go`] recorded, once it has (within ten seconds).
pub(crate) fn started_pid(bin: &Path) -> u32 {
    let began = std::time::Instant::now();
    loop {
        if let Ok(pid) = std::fs::read_to_string(bin.join("started")) {
            return pid.trim().parse().unwrap();
        }
        assert!(
            began.elapsed() < std::time::Duration::from_secs(10),
            "the install never started its toolchain"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Whether process `pid` still exists (a shell builtin: PATH may be bare).
pub(crate) fn alive(pid: u32) -> bool {
    std::process::Command::new("/bin/sh")
        .args(["-c", "kill -0 \"$1\" 2>/dev/null", "sh", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}
