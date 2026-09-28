//! Debug-adapter resolution and provisioning, shared by the clew client
//! (local debugging, `src/dap/adapter.rs`) and clew-server (remote debugging,
//! [`resolve_stdio`]).
//!
//! [`resolve`] maps a language slug (the client's `Lang::slug`) plus a
//! program to the adapter to spawn and the `launch` request body it expects.
//! It never installs anything. An adapter that has to be installed first
//! comes back as [`Resolution::NeedsInstall`], whose
//! [`AdapterInstall::describe`] says exactly what would run; only
//! [`AdapterInstall::install_cancellable`] installs, and callers reach it only
//! through the user's consent — the same rule language-server installs
//! follow. The
//! Debug button used to `pip install` into the user's Python, or download
//! and unpack vscode-js-debug, with no question asked.
//!
//! Every tool is found by absolute path: `PATH` entries that are relative are
//! skipped (see [`store::find_on_path_unresolved`]) and `xcrun` is
//! `/usr/bin/xcrun`. A bare name is resolved by the OS against the inherited
//! `PATH`, where a relative entry would let the directory clew was started in
//! supply the "adapter". A hit is kept as found rather than canonicalized:
//! toolchain managers put shims on `PATH` that dispatch on the name they were
//! run by.
//!
//! A Python program is debugged under the project's own interpreter when it
//! has one — the virtualenv [`langenv`] picks, and hands the language server —
//! so the debugger imports the packages the editor resolves.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{Value, json};

use crate::lsp::langenv;
use crate::lsp::registry::{Archive, Download, Sha256};
use crate::lsp::store;

/// The debugpy release a consented install asks pip for.
pub const DEBUGPY_VERSION: &str = "1.8.14";

/// The vscode-js-debug release clew downloads, verified by digest.
pub const JS_DEBUG_VERSION: &str = "1.117.0";

static JS_DEBUG: Download = Download {
    url: "https://github.com/microsoft/vscode-js-debug/releases/download/v1.117.0/js-debug-dap-v1.117.0.tar.gz",
    sha256: Sha256::from_hex("ad8d04ede9d4b75cc290fd5438a65047a06f786d04f604b6112485b36f090772"),
    archive: Archive::TarGz,
    // The tarball has a top-level `js-debug/`.
    binary: "js-debug/src/dapDebugServer.js",
};

/// How long the `python -c` probe may take. It imports nothing but the
/// standard library; a probe this slow is a hung interpreter.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long `pip install debugpy` may take.
const PIP_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How long `xcrun -f` may take.
const XCRUN_TIMEOUT: Duration = Duration::from_secs(20);

/// How the client talks to the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterTransport {
    /// DAP over the adapter's own stdin/stdout.
    Stdio,
    /// The adapter listens on a loopback TCP port. It is started on port 0 —
    /// the OS picks a free port and the adapter announces it on its stdout —
    /// so the client connects to the port OUR child bound, never to a port
    /// chosen in advance that another process could have taken meanwhile.
    Tcp,
}

/// A resolved adapter: what to spawn, and the `launch` request body.
#[derive(Debug, Clone)]
pub struct AdapterSpec {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub launch: Value,
    pub transport: AdapterTransport,
}

/// The outcome of [`resolve`].
#[derive(Debug, Clone)]
pub enum Resolution {
    Ready(AdapterSpec),
    /// The adapter must be installed first — with the user's consent.
    NeedsInstall(AdapterInstall),
}

/// A debug-adapter install awaiting the user's consent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterInstall {
    /// `python -m pip install debugpy==<DEBUGPY_VERSION>` into THIS
    /// interpreter — the one the adapter will run under; `user` adds
    /// `--user` (not inside a virtualenv, where pip refuses it).
    Debugpy { python: PathBuf, user: bool },
    /// Download the pinned vscode-js-debug, verify its SHA-256, and unpack it
    /// into clew's data directory.
    JsDebug,
}

impl AdapterInstall {
    /// Short name for the consent prompt ("debugpy").
    pub fn name(&self) -> &'static str {
        match self {
            AdapterInstall::Debugpy { .. } => "debugpy",
            AdapterInstall::JsDebug => "vscode-js-debug",
        }
    }

    pub fn version(&self) -> &'static str {
        match self {
            AdapterInstall::Debugpy { .. } => DEBUGPY_VERSION,
            AdapterInstall::JsDebug => JS_DEBUG_VERSION,
        }
    }

    /// Exactly what [`Self::install_cancellable`] will do, for the consent
    /// prompt: the full command line for pip, the URL and digest for a
    /// download.
    pub fn describe(&self) -> String {
        match self {
            AdapterInstall::Debugpy { python, user } => {
                format!("{} {}", python.display(), pip_args(*user).join(" "))
            }
            AdapterInstall::JsDebug => format!(
                "download {} (SHA-256 {}) into clew's data directory",
                JS_DEBUG.url, JS_DEBUG.sha256
            ),
        }
    }

    /// Perform the install, uncancellable — so test-only: the app installs
    /// with [`Self::install_cancellable`], which leaving the project stops.
    #[cfg(test)]
    pub fn install(&self) -> Result<(), String> {
        self.install_cancellable(&AtomicBool::new(false))
    }

    /// Perform the install — only ever after the user consented to
    /// [`Self::describe`]; blocking, so run off the UI thread — abandoned once
    /// `cancel` is set: a download between chunks, a running `pip` killed with
    /// everything it started.
    pub fn install_cancellable(&self, cancel: &AtomicBool) -> Result<(), String> {
        match self {
            AdapterInstall::Debugpy { python, user } => install_debugpy(python, *user, cancel),
            AdapterInstall::JsDebug => install_js_debug(cancel).map(|_| ()),
        }
    }
}

/// Resolve the adapter for `lang` (a slug: native, python, dart, go, node)
/// against this host's environment. Blocking: it may run `xcrun` or a
/// `python -c` probe, each time-boxed. Never installs anything.
///
/// `root` is the project root: where a Python program's virtualenv is looked
/// for, exactly as [`langenv`] looks for the language server's. `cwd` is the
/// debug session's working directory — the root unless the launch
/// configuration names another — and only goes into the `launch` body. The
/// venv used to be looked for in `cwd`, so a launch configuration running a
/// script from `tools/` debugged it under whatever `python3` came first on
/// `PATH`, without the project's packages.
pub fn resolve(
    lang: &str,
    program: &str,
    args: &[String],
    root: &Path,
    cwd: &Path,
) -> Result<Resolution, String> {
    match lang {
        "native" => {
            let command = store::find_on_path_unresolved("lldb-dap")
                .or_else(|| xcrun_find("lldb-dap"))
                .ok_or(
                    "lldb-dap not found — install LLVM (lldb-dap) or the Xcode command-line tools",
                )?;
            Ok(Resolution::Ready(native_spec(command, program, args, cwd)))
        }
        "python" => {
            let python = python_for(root, std::env::var_os("PATH").as_deref())
                .ok_or("python not found on PATH")?;
            python_with(&python, program, args, cwd)
        }
        "dart" => {
            let command = store::find_on_path_unresolved("dart")
                .ok_or("dart not found — install the Dart/Flutter SDK")?;
            Ok(Resolution::Ready(dart_spec(command, program, args, cwd)))
        }
        "go" => {
            let command = store::find_on_path_unresolved("dlv").ok_or(
                "dlv not found — run: go install github.com/go-delve/delve/cmd/dlv@latest",
            )?;
            Ok(Resolution::Ready(go_spec(command, program, args, cwd)))
        }
        "node" => {
            let node =
                store::find_on_path_unresolved("node").ok_or("node not found — install Node.js")?;
            Ok(node_with(node, program, args, cwd))
        }
        other => Err(format!("no debug adapter for {other}")),
    }
}

/// The interpreter to debug a Python program in `root` under: the project's
/// own virtualenv, exactly as [`langenv`] picks it for the language server —
/// and held to the same check of what executing it would run — else the first
/// `python3` (then `python`) on `path`.
///
/// Every candidate is kept as found, never resolved: a virtualenv's
/// `bin/python` is a symlink to the base interpreter, and Python finds its
/// environment from the path it was run by, so following the link would
/// debug under the base install instead of the venv whose packages the
/// program imports.
fn python_for(root: &Path, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    langenv::find_python_interpreter(root).or_else(|| {
        let path = path?;
        store::find_in_unresolved(path, "python3")
            .or_else(|| store::find_in_unresolved(path, "python"))
    })
}

/// lldb-dap for native (Rust/C/C++) binaries. Ships with Xcode / LLVM.
fn native_spec(command: PathBuf, program: &str, args: &[String], cwd: &Path) -> AdapterSpec {
    AdapterSpec {
        command,
        args: Vec::new(),
        launch: json!({
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "stopOnEntry": false,
        }),
        transport: AdapterTransport::Stdio,
    }
}

/// debugpy: `python -m debugpy.adapter` speaks DAP over stdio and debugs the
/// same interpreter it runs under.
fn python_with(
    python: &Path,
    program: &str,
    args: &[String],
    cwd: &Path,
) -> Result<Resolution, String> {
    let probe = probe_python(python)?;
    if !probe.has_debugpy {
        return Ok(Resolution::NeedsInstall(AdapterInstall::Debugpy {
            python: python.to_path_buf(),
            user: !probe.in_venv,
        }));
    }
    Ok(Resolution::Ready(AdapterSpec {
        command: python.to_path_buf(),
        args: vec!["-m".into(), "debugpy.adapter".into()],
        launch: json!({
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "console": "internalConsole",
            "python": [python.to_string_lossy()],
            "stopOnEntry": false,
            "justMyCode": false,
        }),
        transport: AdapterTransport::Stdio,
    }))
}

/// Dart's SDK ships a DAP adapter: `dart debug_adapter` over stdio.
fn dart_spec(command: PathBuf, program: &str, args: &[String], cwd: &Path) -> AdapterSpec {
    AdapterSpec {
        command,
        args: vec!["debug_adapter".into()],
        launch: json!({
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "toolArgs": [],
        }),
        transport: AdapterTransport::Stdio,
    }
}

/// Delve for Go: `dlv dap` (mode=debug compiles and runs the package). It
/// listens on a TCP port: port 0, announced as `DAP server listening at:
/// <addr>`.
fn go_spec(command: PathBuf, program: &str, args: &[String], cwd: &Path) -> AdapterSpec {
    AdapterSpec {
        command,
        args: vec!["dap".into(), "--listen".into(), "127.0.0.1:0".into()],
        launch: json!({
            "request": "launch",
            "mode": "debug",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
        }),
        transport: AdapterTransport::Tcp,
    }
}

/// vscode-js-debug for JS/TS: `node dapDebugServer.js <port> <host>` listens
/// on TCP — port 0, announced as `Debug server listening at <addr>`. It is
/// provisioned under clew's data dir; until it is, this is a
/// [`Resolution::NeedsInstall`].
fn node_with(node: PathBuf, program: &str, args: &[String], cwd: &Path) -> Resolution {
    let Some(server) = js_debug_server() else {
        return Resolution::NeedsInstall(AdapterInstall::JsDebug);
    };
    Resolution::Ready(AdapterSpec {
        command: node,
        args: vec![
            server.to_string_lossy().into_owned(),
            "0".into(),
            "127.0.0.1".into(),
        ],
        launch: json!({
            "type": "pwa-node",
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "console": "internalConsole",
        }),
        transport: AdapterTransport::Tcp,
    })
}

/// What a `python -c` probe learned about an interpreter.
#[derive(Debug, PartialEq, Eq)]
struct PythonProbe {
    /// `debugpy` is importable (found, not imported: nothing of it runs).
    has_debugpy: bool,
    /// Running inside a virtualenv (`sys.prefix != sys.base_prefix`).
    in_venv: bool,
}

const PROBE: &str = "import sys, importlib.util\n\
print(int(importlib.util.find_spec('debugpy') is not None), \
int(sys.prefix != getattr(sys, 'base_prefix', sys.prefix)))";

/// Ask `python` about itself, time-boxed.
fn probe_python(python: &Path) -> Result<PythonProbe, String> {
    probe_python_within(python, PROBE_TIMEOUT)
}

fn probe_python_within(python: &Path, limit: Duration) -> Result<PythonProbe, String> {
    let mut cmd = Command::new(python);
    cmd.args(["-c", PROBE]);
    neutral(&mut cmd);
    let out = store::run_bounded(cmd, limit)
        .map_err(|e| format!("could not probe {}: {e}", python.display()))?;
    if !out.status.success() {
        return Err(format!("{} could not be run as Python 3", python.display()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.split_whitespace();
    match (fields.next(), fields.next()) {
        (Some(debugpy), Some(venv)) => Ok(PythonProbe {
            has_debugpy: debugpy == "1",
            in_venv: venv == "1",
        }),
        _ => Err(format!("unexpected answer from {}", python.display())),
    }
}

/// Run a Python helper where no project can inject modules into it. `-c`
/// and `-m` put the working directory first on `sys.path`, so a `debugpy/`
/// or `pip/` directory in whatever directory clew was started from would be
/// imported instead of the real one — its code run before the user has
/// agreed to anything. The root directory holds no such thing, and
/// `PYTHONSAFEPATH` (3.11+) drops the entry altogether.
fn neutral(cmd: &mut Command) {
    let dir = if cfg!(unix) {
        PathBuf::from("/")
    } else {
        std::env::temp_dir()
    };
    cmd.current_dir(dir).env("PYTHONSAFEPATH", "1");
}

/// pip's arguments for the debugpy install, pinned. Rendered into the
/// consent prompt as they are passed.
fn pip_args(user: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        "-m",
        "pip",
        "install",
        "--disable-pip-version-check",
        "--no-input",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if user {
        args.push("--user".into());
    }
    args.push(format!("debugpy=={DEBUGPY_VERSION}"));
    args
}

fn pip_command(python: &Path, user: bool) -> Command {
    let mut cmd = Command::new(python);
    cmd.args(pip_args(user));
    neutral(&mut cmd);
    cmd
}

fn install_debugpy(python: &Path, user: bool, cancel: &AtomicBool) -> Result<(), String> {
    let out = store::run_bounded_cancellable(pip_command(python, user), PIP_TIMEOUT, cancel)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("pip failed");
        return Err(format!(
            "pip install debugpy=={DEBUGPY_VERSION}: {}",
            last.trim()
        ));
    }
    // Confirm with the interpreter the adapter will actually run under.
    if probe_python(python)?.has_debugpy {
        Ok(())
    } else {
        Err(format!(
            "installed debugpy but {} still can't import it",
            python.display()
        ))
    }
}

/// Root under clew's data dir where downloadable debug adapters live.
fn adapters_root() -> Option<PathBuf> {
    Some(store::data_root()?.join("debug-adapters"))
}

/// The versioned install directory for vscode-js-debug.
fn js_debug_dir() -> Option<PathBuf> {
    Some(
        adapters_root()?
            .join("vscode-js-debug")
            .join(JS_DEBUG_VERSION),
    )
}

/// The provisioned vscode-js-debug DAP server entrypoint, if installed.
///
/// Installs are staged and swapped in whole (see `lsp::store`), so the
/// entrypoint existing means the whole tree does — the old in-place unpack
/// could leave a half-extracted adapter that passed this check forever.
pub fn js_debug_server() -> Option<PathBuf> {
    let path = js_debug_dir()?.join(JS_DEBUG.binary);
    path.is_file().then_some(path)
}

/// Download, verify (pinned SHA-256), and unpack vscode-js-debug. Blocking.
fn install_js_debug(cancel: &AtomicBool) -> Result<PathBuf, String> {
    let root = adapters_root().ok_or("no data directory")?;
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let dest = js_debug_dir().ok_or("no data directory")?;
    store::install_download(&JS_DEBUG, &dest, &root, cancel)
}

/// Locate a tool in the active Xcode toolchain via `xcrun -f`, by absolute
/// path: a bare `xcrun` was looked up on the inherited PATH.
fn xcrun_find(tool: &str) -> Option<PathBuf> {
    const XCRUN: &str = "/usr/bin/xcrun";
    if !cfg!(target_os = "macos") || !Path::new(XCRUN).is_file() {
        return None;
    }
    let mut cmd = Command::new(XCRUN);
    cmd.args(["-f", tool]).current_dir("/");
    let out = store::run_bounded(cmd, XCRUN_TIMEOUT).ok()?;
    if !out.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    (path.is_absolute() && path.is_file()).then_some(path)
}

/// A resolved stdio adapter for clew-server: what to spawn, and the `launch`
/// request body serialized (the client passes it to the adapter verbatim,
/// with paths that are native to THIS host).
#[derive(Debug)]
pub struct StdioAdapter {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub launch: String,
}

/// The server's view of [`resolve`], for adapters a remote host can proxy
/// over the protocol:
///   - stdio adapters only (lldb-dap, debugpy, `dart debug_adapter`): their
///     frames proxy over `ProcessOutput`/`ProcessInput`. TCP adapters
///     (delve, vscode-js-debug) listen on the REMOTE's loopback, which the
///     client cannot reach — refused with a clear message.
///   - no provisioning: installing on a remote host without the user's
///     explicit consent would break the rule `LspInstall` exists for. The
///     error names what to install instead.
///
/// `root` is the project root, which is also the session's working directory:
/// `SpawnAdapter` carries no other.
pub fn resolve_stdio(
    lang: &str,
    program: &str,
    args: &[String],
    root: &Path,
) -> Result<StdioAdapter, String> {
    let tcp_refusal = || {
        format!(
            "the {lang} debug adapter speaks DAP over a TCP port on this host, \
             which a remote client cannot reach — remote debugging currently \
             supports native (lldb), Python, and Dart"
        )
    };
    if matches!(lang, "go" | "node") {
        return Err(tcp_refusal());
    }
    match resolve(lang, program, args, root, root)
        .map_err(|e| format!("on the remote host: {e}"))?
    {
        Resolution::Ready(spec) if spec.transport == AdapterTransport::Stdio => Ok(StdioAdapter {
            command: spec.command,
            args: spec.args,
            launch: spec.launch.to_string(),
        }),
        Resolution::Ready(_) => Err(tcp_refusal()),
        Resolution::NeedsInstall(install) => Err(format!(
            "{} is not installed on the remote host — install it there: {}",
            install.name(),
            install.describe()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::store::testing::{script, with_store};
    use crate::testutil::{EnvVars, TempDir};

    #[test]
    fn tcp_adapters_are_refused_with_a_reason() {
        for lang in ["go", "node"] {
            let err = resolve_stdio(lang, "/proj/main", &[], Path::new("/proj")).unwrap_err();
            assert!(err.contains("TCP"), "{err}");
        }
        assert!(
            resolve_stdio("cobol", "/p", &[], Path::new("/"))
                .unwrap_err()
                .contains("no debug adapter")
        );
    }

    /// F3: consent is given to the prompt's text, so every argument pip runs
    /// with must be in it — including the pin, which used to be absent (an
    /// unpinned `pip install --user debugpy`, run with no prompt at all).
    #[test]
    fn the_pip_argv_is_what_the_consent_prompt_describes() {
        for user in [true, false] {
            let python = PathBuf::from("/opt/py/bin/python3");
            let install = AdapterInstall::Debugpy {
                python: python.clone(),
                user,
            };
            let describe = install.describe();
            let described: Vec<&str> = describe.split_whitespace().collect();
            let cmd = pip_command(&python, user);
            assert_eq!(cmd.get_program(), python.as_os_str());
            assert_eq!(described.first(), Some(&"/opt/py/bin/python3"));
            for arg in cmd.get_args() {
                let arg = arg.to_str().unwrap();
                assert!(
                    described.contains(&arg),
                    "`{arg}` runs but is not in the prompt"
                );
            }
            let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
            assert!(args.contains(&format!("debugpy=={DEBUGPY_VERSION}").as_str()));
            assert_eq!(args.contains(&"--user"), user);
            // Never run from the project: `-m pip` imports from the cwd.
            assert_eq!(cmd.get_current_dir(), Some(Path::new("/")));
        }
        let js = AdapterInstall::JsDebug.describe();
        assert!(
            js.contains(JS_DEBUG.url) && js.contains(&JS_DEBUG.sha256.to_hex()),
            "{js}"
        );
    }

    /// A fake interpreter: answers the probe from `probe_answer` (a shell
    /// expression printing "<has_debugpy> <in_venv>"), and records where it
    /// ran and with what.
    #[cfg(unix)]
    fn fake_python(dir: &Path, probe_answer: &str) -> PathBuf {
        let log = dir.join("log");
        let python = script(
            &dir.join("bin/python3"),
            &format!(
                "case \"$1\" in -c|-m) echo \"$(pwd) $PYTHONSAFEPATH $*\" >> '{log}' ;; esac\n\
                 case \"$1\" in\n\
                   -c) {probe_answer} ;;\n\
                   -m) touch '{mark}' ;;\n\
                 esac",
                log = log.display(),
                mark = dir.join("installed").display(),
            ),
        );
        // Run bare (it does nothing then, and logs nothing), so the probe's
        // deadline times the probe and not the OS's check of a new file.
        crate::testutil::settle_new_executable(&python);
        python
    }

    /// F3/F14: a missing debugpy is a consent request, not an install; the
    /// venv decides `--user`; the interpreter path is kept exactly as found.
    #[test]
    #[cfg(unix)]
    fn a_missing_debugpy_is_a_consent_request() {
        let dir = TempDir::new("dap-py");
        let python = fake_python(&dir, "echo '0 1'");
        let got = python_with(&python, "/p/main.py", &[], Path::new("/p")).unwrap();
        match got {
            Resolution::NeedsInstall(AdapterInstall::Debugpy { python: p, user }) => {
                assert_eq!(p, python, "the interpreter as found, not resolved");
                assert!(!user, "no --user inside a virtualenv");
            }
            other => panic!("expected a consent request, got {other:?}"),
        }
        assert!(
            !dir.join("installed").exists(),
            "resolving must not install"
        );
        // The probe ran from `/`, with the safe-path flag, never the project.
        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        assert!(log.starts_with("/ 1 -c "), "{log}");
    }

    #[test]
    #[cfg(unix)]
    fn a_present_debugpy_resolves_to_the_same_interpreter() {
        let dir = TempDir::new("dap-py-ok");
        let python = fake_python(&dir, "echo '1 0'");
        let Resolution::Ready(spec) =
            python_with(&python, "/p/main.py", &["--x".into()], Path::new("/p")).unwrap()
        else {
            panic!("debugpy is present");
        };
        assert_eq!(spec.command, python);
        assert_eq!(spec.args, ["-m", "debugpy.adapter"]);
        assert_eq!(spec.transport, AdapterTransport::Stdio);
        assert_eq!(spec.launch["python"][0], python.to_string_lossy().as_ref());
        assert_eq!(spec.launch["args"][0], "--x");
    }

    /// The consented install runs pip with the pin, then re-probes the same
    /// interpreter before reporting success.
    #[test]
    #[cfg(unix)]
    fn the_consented_install_runs_pip_and_confirms_it() {
        let dir = TempDir::new("dap-pip");
        let mark = dir.join("installed");
        let python = fake_python(
            &dir,
            &format!(
                "if [ -f '{}' ]; then echo '1 0'; else echo '0 0'; fi",
                mark.display()
            ),
        );
        let install = AdapterInstall::Debugpy {
            python: python.clone(),
            user: true,
        };
        install.install().unwrap();
        assert!(mark.exists());
        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        assert!(
            log.contains(&format!("-m pip install --disable-pip-version-check --no-input --user debugpy=={DEBUGPY_VERSION}")),
            "{log}"
        );
    }

    /// A fake virtualenv in `dir/project`: `.venv/bin/python` links to a
    /// base interpreter outside the project (as `python -m venv` makes it)
    /// that has debugpy. Returns the project and the venv's interpreter.
    #[cfg(unix)]
    fn project_with_venv(dir: &Path) -> (PathBuf, PathBuf) {
        let base = fake_python(&dir.join("base"), "echo '1 1'");
        let project = dir.join("project");
        let venv_bin = project.join(".venv/bin");
        std::fs::create_dir_all(&venv_bin).unwrap();
        std::os::unix::fs::symlink(&base, venv_bin.join("python")).unwrap();
        (project, venv_bin.join("python"))
    }

    /// F10: a Python program is debugged under the project's virtualenv —
    /// the interpreter its language server is given — not whatever `python3`
    /// comes first on PATH, which lacks the project's packages. The venv's
    /// path is kept as found: its `bin/python` links to the base install.
    #[test]
    #[cfg(unix)]
    fn python_debugging_uses_the_project_venv() {
        // An activated virtualenv in the developer's shell is a candidate
        // too (`VIRTUAL_ENV`); this is about the project's and PATH's.
        let _env = EnvVars::new().remove("VIRTUAL_ENV");
        let dir = TempDir::new("dap-venv");
        // The project's venv, linked to a base interpreter outside it…
        let (project, venv_python) = project_with_venv(&dir);
        // …and an unrelated python3 first on PATH.
        let elsewhere = fake_python(&dir.join("elsewhere"), "echo '1 0'");
        let search = std::env::join_paths([elsewhere.parent().unwrap()]).unwrap();

        let chosen = python_for(&project, Some(&search)).unwrap();
        assert_eq!(chosen, venv_python, "the venv, as found");
        let Resolution::Ready(spec) = python_with(&chosen, "/p/main.py", &[], &project).unwrap()
        else {
            panic!("debugpy is present in the fake venv");
        };
        assert_eq!(spec.command, venv_python);
        assert_eq!(spec.launch["python"][0], chosen.to_string_lossy().as_ref());

        // Without a venv, PATH decides.
        let bare = dir.join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(python_for(&bare, Some(&search)), Some(elsewhere));
    }

    /// The venv is looked for at the PROJECT ROOT, not in the session's
    /// working directory: a launch configuration that runs a script from a
    /// subdirectory used to be debugged under whatever `python3` came first
    /// on PATH. The working directory still goes into the launch body.
    #[test]
    #[cfg(unix)]
    fn a_subdirectory_cwd_still_debugs_under_the_project_venv() {
        // (The project's `.venv` is looked for before an activated
        // `VIRTUAL_ENV`, so the developer's shell cannot decide this one.)
        let dir = TempDir::new("dap-venv-cwd");
        let (project, venv_python) = project_with_venv(&dir);
        let cwd = project.join("tools");
        std::fs::create_dir_all(&cwd).unwrap();
        let program = cwd.join("run.py");
        let resolved = resolve(
            "python",
            &program.to_string_lossy(),
            &["--fast".into()],
            &project,
            &cwd,
        )
        .unwrap();
        let Resolution::Ready(spec) = resolved else {
            panic!("debugpy is present in the project's venv: {resolved:?}");
        };
        assert_eq!(spec.command, venv_python, "not the project's interpreter");
        assert_eq!(spec.launch["cwd"], cwd.to_string_lossy().as_ref());
        assert_eq!(spec.launch["program"], program.to_string_lossy().as_ref());
        assert_eq!(spec.launch["args"][0], "--fast");
    }

    /// F10: a debugpy install stops when cancelled, with what it started,
    /// instead of running out its ten-minute limit.
    #[test]
    #[cfg(unix)]
    fn a_cancelled_debugpy_install_stops() {
        let dir = TempDir::new("dap-pip-cancel");
        let python = script(
            &dir.join("bin/python3"),
            "case \"$1\" in -m) sleep 60 ;; *) echo '0 0' ;; esac",
        );
        crate::testutil::settle_new_executable(&python);
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flip = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        };
        let started = std::time::Instant::now();
        let install = AdapterInstall::Debugpy { python, user: true };
        let err = install.install_cancellable(&cancel).unwrap_err();
        flip.join().unwrap();
        assert!(err.contains("cancelled"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// A hung interpreter fails the probe instead of hanging the debugger's
    /// start (the probe had no timeout).
    #[test]
    #[cfg(unix)]
    fn a_hung_interpreter_fails_the_probe() {
        let dir = TempDir::new("dap-py-hang");
        let python = fake_python(&dir, "sleep 30");
        let started = std::time::Instant::now();
        let err = probe_python_within(&python, Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("did not finish"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// F3/F9/F10: js-debug is a consent request until installed (into a
    /// versioned directory of its own), and then listens on an OS-chosen
    /// port — no fixed fallback port.
    #[test]
    fn js_debug_needs_consent_then_listens_on_an_os_chosen_port() {
        with_store("js-debug", |dir| {
            let node = PathBuf::from("/usr/local/bin/node");
            assert!(matches!(
                node_with(node.clone(), "/p/app.js", &[], Path::new("/p")),
                Resolution::NeedsInstall(AdapterInstall::JsDebug)
            ));
            let entry = dir
                .join("debug-adapters/vscode-js-debug")
                .join(JS_DEBUG_VERSION)
                .join(JS_DEBUG.binary);
            std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
            std::fs::write(&entry, b"// server").unwrap();
            let Resolution::Ready(spec) = node_with(node, "/p/app.js", &[], Path::new("/p")) else {
                panic!("installed now");
            };
            assert_eq!(spec.transport, AdapterTransport::Tcp);
            assert_eq!(spec.args[0], entry.to_string_lossy());
            assert_eq!(&spec.args[1..], ["0", "127.0.0.1"]);
        });
        let go = go_spec(PathBuf::from("/bin/dlv"), "/p", &[], Path::new("/p"));
        assert_eq!(go.args, ["dap", "--listen", "127.0.0.1:0"]);
        assert_eq!(go.transport, AdapterTransport::Tcp);
    }

    /// The js-debug download goes through the store's verified, staged
    /// installer — refused outside the data dir before any network access.
    #[test]
    fn the_js_debug_download_is_confined_to_the_data_dir() {
        with_store("js-debug-confined", |dir| {
            let outside = dir.join("elsewhere");
            let err = store::install_download(
                &JS_DEBUG,
                &outside,
                &dir.join("debug-adapters"),
                &AtomicBool::new(false),
            )
            .unwrap_err();
            assert!(err.contains("outside the store"), "{err}");
        });
    }
}
