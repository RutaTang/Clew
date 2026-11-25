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

/// The debugpy release clew downloads, verified by digest: its pure-Python
/// wheel, the same for every platform and Python 3 release. Run from clew's
/// data directory, it debugs a program under any interpreter: nothing is
/// installed into the project's environment, which may have no pip (one uv
/// made) or refuse to take packages (a system Python, PEP 668).
pub const DEBUGPY_VERSION: &str = "1.8.22";

static DEBUGPY: Download = Download {
    url: "https://files.pythonhosted.org/packages/56/3d/c7dc9f35bc9e22cdb53679f844bee40fd5cff871862f60595869a433400d/debugpy-1.8.22-py2.py3-none-any.whl",
    sha256: Sha256::from_hex("a9e9d3550e15ca479c59333e90845029190531f0cacfedab3b815a57bd913947"),
    archive: Archive::Zip,
    // A wheel is a zip of the package: `debugpy/` at its top.
    binary: "debugpy/adapter/__main__.py",
};

/// The pytest release a consented install asks pip for: what runs a Python
/// project's tests when its interpreter has none.
pub const PYTEST_VERSION: &str = "8.3.5";

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

/// How long `pip install pytest` may take.
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
    /// As [`Self::Tcp`], for an adapter that IS the debuggee (Ruby's `rdbg`
    /// runs the program in its own process): it announces its port on
    /// stderr, and its stdout — and the rest of its stderr — is the program's
    /// output.
    TcpOnStderr,
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
    /// Download the pinned debugpy, verify its SHA-256, and unpack it into
    /// clew's data directory. It runs under the program's own interpreter,
    /// which gets nothing installed.
    Debugpy,
    /// Install `pytest==<PYTEST_VERSION>` into the interpreter a test runs
    /// under, `via` pip or uv: without it a test file only defines its
    /// tests, and one test cannot be run alone at all.
    Pytest { python: PathBuf, via: PyInstaller },
    /// Download the pinned vscode-js-debug, verify its SHA-256, and unpack it
    /// into clew's data directory.
    JsDebug,
    /// Install the packages a script lists in its inline metadata (PEP 723,
    /// `# /// script`) into the environment `uv` keeps for it
    /// (`uv sync --script`): what `uv run` would run it in.
    ScriptEnv { uv: PathBuf, script: PathBuf },
    /// Install a JavaScript project's own dependencies with its package
    /// manager (`pm install` in `dir`): its test runner, `tsx` and what its
    /// programs import are among them. The project's own install scripts
    /// run with it, as they do when it is installed by hand.
    JsDependencies { pm: PathBuf, dir: PathBuf },
}

/// What installs a package into a Python environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PyInstaller {
    /// `python -m pip install`; `user` adds `--user`, for an interpreter in
    /// no environment of its own (pip refuses it inside one).
    Pip { user: bool },
    /// `uv pip install --python <python>`: an environment uv made has no
    /// pip of its own.
    Uv(PathBuf),
}

impl AdapterInstall {
    /// Short name for the consent prompt ("debugpy").
    pub fn name(&self) -> &'static str {
        match self {
            AdapterInstall::Debugpy => "debugpy",
            AdapterInstall::Pytest { .. } => "pytest",
            AdapterInstall::JsDebug => "vscode-js-debug",
            AdapterInstall::ScriptEnv { .. } => "uv",
            AdapterInstall::JsDependencies { .. } => "dependencies",
        }
    }

    pub fn version(&self) -> &'static str {
        match self {
            AdapterInstall::Debugpy => DEBUGPY_VERSION,
            AdapterInstall::Pytest { .. } => PYTEST_VERSION,
            AdapterInstall::JsDebug => JS_DEBUG_VERSION,
            // Whatever the script (or the project) asks for.
            AdapterInstall::ScriptEnv { .. } | AdapterInstall::JsDependencies { .. } => "",
        }
    }

    /// Whether this installs the packages a script lists (PEP 723): what
    /// the consent prompt and the status line then say.
    pub fn is_script_packages(&self) -> bool {
        matches!(self, AdapterInstall::ScriptEnv { .. })
    }

    /// Whether this installs a project's own dependencies: what the consent
    /// prompt and the status line then say.
    pub fn is_dependencies(&self) -> bool {
        matches!(self, AdapterInstall::JsDependencies { .. })
    }

    /// Whether this installs what runs tests rather than a debugger: what
    /// the consent prompt and the status line then name it.
    pub fn is_test_runner(&self) -> bool {
        matches!(self, AdapterInstall::Pytest { .. })
    }

    /// Whether this downloads into clew's own data directory, rather than
    /// installing into the project's environment.
    pub fn is_download(&self) -> bool {
        matches!(self, AdapterInstall::Debugpy | AdapterInstall::JsDebug)
    }

    /// Exactly what [`Self::install_cancellable`] will do, for the consent
    /// prompt: the full command line for pip, the URL and digest for a
    /// download.
    pub fn describe(&self) -> String {
        match self {
            AdapterInstall::Debugpy => format!(
                "download {} (SHA-256 {}) into Clew's data directory",
                DEBUGPY.url, DEBUGPY.sha256
            ),
            AdapterInstall::Pytest { python, via } => {
                let (program, args) = pytest_argv(python, via);
                format!("{} {}", program.display(), args.join(" "))
            }
            AdapterInstall::JsDebug => format!(
                "download {} (SHA-256 {}) into Clew's data directory",
                JS_DEBUG.url, JS_DEBUG.sha256
            ),
            AdapterInstall::ScriptEnv { uv, script } => {
                format!("{} sync --script {}", uv.display(), script.display())
            }
            AdapterInstall::JsDependencies { pm, dir } => {
                format!("cd {} && {} install", dir.display(), pm.display())
            }
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
            AdapterInstall::Debugpy => install_debugpy(cancel).map(|_| ()),
            AdapterInstall::Pytest { python, via } => install_pytest(python, via, cancel),
            AdapterInstall::JsDebug => install_js_debug(cancel).map(|_| ()),
            AdapterInstall::ScriptEnv { uv, script } => install_script_env(uv, script, cancel),
            AdapterInstall::JsDependencies { pm, dir } => install_js_dependencies(pm, dir, cancel),
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
/// `PATH`, without the project's packages. A Python program is debugged under
/// the interpreter the user chose for the project (`choices`), when they
/// chose one and it can be used.
pub fn resolve(
    lang: &str,
    program: &str,
    args: &[String],
    root: &Path,
    cwd: &Path,
    choices: &clew_protocol::LanguageChoices,
    server: Option<&Path>,
) -> Result<Resolution, String> {
    match lang {
        "native" => {
            let command = lldb_dap_names()
                .iter()
                .find_map(|name| store::find_on_path_unresolved(name))
                .or_else(|| xcrun_find("lldb-dap"))
                .ok_or(
                    "lldb-dap not found — install LLVM (lldb-dap) or the Xcode command-line tools",
                )?;
            // A Zig source file is built for the session, through LLVM: its
            // debug information is then one lldb reads variables from.
            let binary = match is_zig_source(program) {
                true => build_zig(&zig_for(choices)?, &in_dir(cwd, program), cwd)?,
                false => PathBuf::from(program),
            };
            // lldb-dap runs a command written `?command` without echoing it
            // into the program's output; lldb-vscode, its name before LLVM
            // 18, knows no such prefix.
            let quiet = command
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("lldb-dap"));
            let mut spec = native_spec(command, &binary.to_string_lossy(), args, cwd);
            match dwarf_language(&in_dir(cwd, &binary.to_string_lossy())) {
                Some(DW_LANG_ZIG) => spec.launch[NOTICE] = json!(ZIG_BACKEND_NOTICE),
                Some(DW_LANG_RUST) => {
                    rust_setup(&mut spec.launch, rust_sysroot(cwd).as_deref(), quiet)
                }
                _ => {}
            }
            Ok(Resolution::Ready(spec))
        }
        "python" => {
            // A script that lists the packages it needs (PEP 723) runs in
            // the environment uv keeps for it, as `uv run` runs it: run in
            // the project's, its imports failed.
            let script = in_dir(cwd, program);
            let path = crate::linerpc::spawn_path();
            let mut notice = None;
            let own_env = match lists_its_packages(&script) {
                false => None,
                true => match path
                    .as_deref()
                    .and_then(|path| store::find_in_unresolved(path, "uv"))
                {
                    Some(uv) => match script_env(&uv, &script) {
                        Some(env) => Some(env),
                        None => {
                            return Ok(Resolution::NeedsInstall(AdapterInstall::ScriptEnv {
                                uv,
                                script,
                            }));
                        }
                    },
                    None => {
                        let msg = "This script lists the packages it needs (# /// script), and uv isn't installed to install them, so it runs with the project's Python. Install uv to run it with its own packages."
                            .to_string();
                        notice = Some(msg);
                        None
                    }
                },
            };
            let python = match own_env {
                Some(env) => env,
                None => python_for(root, choices.python.as_deref(), path.as_deref())
                    .ok_or("python not found on PATH")?,
            };
            let mut resolved = python_with(&python, program, args, cwd)?;
            if let (Some(notice), Resolution::Ready(spec)) = (notice, &mut resolved) {
                spec.launch[NOTICE] = json!(notice);
            }
            Ok(resolved)
        }
        "dart" => {
            let command = match chosen_sdk(choices, "dart") {
                Some(chosen) => store::find_program(chosen)
                    .map_err(|why| format!("the Dart SDK you chose cannot be run: {why}"))?,
                None => store::find_on_path_unresolved("dart")
                    .ok_or("dart not found — install the Dart/Flutter SDK")?,
            };
            Ok(Resolution::Ready(dart_spec(command, program, args, cwd)))
        }
        "go" => {
            let command = store::find_on_path_unresolved("dlv").ok_or(
                "dlv not found — run: go install github.com/go-delve/delve/cmd/dlv@latest",
            )?;
            let tags = choices.go.as_ref().and_then(langenv::go_tags_flag);
            Ok(Resolution::Ready(go_spec(
                command,
                program,
                args,
                cwd,
                tags.as_deref(),
            )))
        }
        "node" => {
            let node =
                store::find_on_path_unresolved("node").ok_or("node not found — install Node.js")?;
            // A script is debugged under the runtime its project runs it
            // with (Run's: `jsrun::main_command`).
            let script = in_dir(cwd, program);
            let is_script = matches!(
                script.extension().and_then(|e| e.to_str()),
                Some("js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx")
            );
            match is_script.then(|| crate::jsrun::runtime_of(root, &script)) {
                Some("deno") => {
                    let deno = store::find_on_path_unresolved("deno")
                        .ok_or("deno not found — install Deno")?;
                    Ok(deno_with(node, &deno, program, args, cwd))
                }
                Some("bun") => {
                    let mut resolution = node_with(node, program, args, cwd);
                    if let Resolution::Ready(spec) = &mut resolution {
                        spec.launch[NOTICE] = json!(BUN_NOTICE);
                    }
                    Ok(resolution)
                }
                _ => Ok(node_with(node, program, args, cwd)),
            }
        }
        "browser" => {
            // js-debug runs under Node and drives the browser itself.
            let node =
                store::find_on_path_unresolved("node").ok_or("node not found — install Node.js")?;
            Ok(browser_with(node, program, cwd))
        }
        "ruby" => {
            // The Ruby whose Ruby LSP the user chose has its own `rdbg`, its
            // gems' Ruby: debugged under it, as the editor reads the code.
            let beside_chosen = chosen_sdk(choices, "ruby")
                .and_then(|chosen| Path::new(chosen.trim()).parent().map(|d| d.join("rdbg")))
                .filter(|rdbg| rdbg.is_absolute() && rdbg.is_file());
            let command = match beside_chosen {
                Some(rdbg) => rdbg,
                None => store::find_on_path_unresolved("rdbg")
                    .ok_or("rdbg not found — install Ruby's debugger: gem install debug")?,
            };
            Ok(Resolution::Ready(ruby_spec(command, program, args, cwd)))
        }
        "csharp" => {
            let command = store::find_on_path_unresolved("netcoredbg").ok_or(
                "netcoredbg not found — install it (github.com/Samsung/netcoredbg) and put it on the PATH",
            )?;
            Ok(Resolution::Ready(csharp_spec(command, program, args, cwd)))
        }
        "php" => {
            let php = store::find_on_path_unresolved("php")
                .ok_or("php not found — install PHP, with Xdebug 3")?;
            let server = server.ok_or("clew's PHP debug adapter (clew-server) is not available")?;
            Ok(Resolution::Ready(php_spec(
                server, &php, program, args, cwd,
            )))
        }
        "java" => {
            // The JDK chosen for the project (Settings → Language servers), else
            // the PATH's.
            let chosen = choices
                .java
                .as_deref()
                .map(|home| Path::new(home.trim()).join("bin"))
                .filter(|bin| bin.is_absolute() && bin.join("java").is_file());
            let (java, javac) = match chosen {
                Some(bin) => (bin.join("java"), bin.join("javac")),
                None => (
                    store::find_on_path_unresolved("java")
                        .ok_or("java not found — install a JDK")?,
                    store::find_on_path_unresolved("javac")
                        .ok_or("javac not found — install a JDK (not only a JRE)")?,
                ),
            };
            let server =
                server.ok_or("clew's Java debug adapter (clew-server) is not available")?;
            let args_out = vec![
                "java-debug".to_string(),
                "--java".to_string(),
                java.to_string_lossy().into_owned(),
                "--javac".to_string(),
                javac.to_string_lossy().into_owned(),
            ];
            Ok(Resolution::Ready(own_spec(
                server, args_out, program, args, cwd,
            )))
        }
        "erlang" => {
            let erl = store::find_on_path_unresolved("erl")
                .ok_or("erl not found — install Erlang/OTP, with its debugger application")?;
            let erlc = store::find_on_path_unresolved("erlc")
                .ok_or("erlc not found — install Erlang/OTP")?;
            let server =
                server.ok_or("clew's Erlang debug adapter (clew-server) is not available")?;
            let args_out = vec![
                "erlang-debug".to_string(),
                "--erl".to_string(),
                erl.to_string_lossy().into_owned(),
                "--erlc".to_string(),
                erlc.to_string_lossy().into_owned(),
            ];
            Ok(Resolution::Ready(own_spec(
                server, args_out, program, args, cwd,
            )))
        }
        "haskell" => {
            // The GHC whose language server the user chose (ghcup keeps
            // them side by side), else the PATH's.
            let beside_chosen = chosen_sdk(choices, "haskell")
                .and_then(|chosen| Path::new(chosen.trim()).parent().map(|d| d.join("ghci")))
                .filter(|ghci| ghci.is_absolute() && ghci.is_file());
            let ghci = match beside_chosen {
                Some(ghci) => ghci,
                None => store::find_on_path_unresolved("ghci")
                    .ok_or("ghci not found — install GHC (ghcup install ghc)")?,
            };
            let server =
                server.ok_or("clew's Haskell debug adapter (clew-server) is not available")?;
            let args_out = vec![
                "haskell-debug".to_string(),
                "--ghci".to_string(),
                ghci.to_string_lossy().into_owned(),
            ];
            Ok(Resolution::Ready(own_spec(
                server, args_out, program, args, cwd,
            )))
        }
        other => Err(format!("no debug adapter for {other}")),
    }
}

/// Most bytes of a script read for its inline metadata.
const MAX_SCRIPT_BYTES: u64 = 4 * 1024 * 1024;

/// Whether `script` lists the packages it needs in its inline metadata
/// (PEP 723): a `# /// script` line, closed by a `# ///` line.
pub fn lists_its_packages(script: &Path) -> bool {
    if script.extension().is_none_or(|e| e != "py") {
        return false;
    }
    let Some(text) = crate::statefile::read_capped(script, MAX_SCRIPT_BYTES) else {
        return false;
    };
    let mut lines = text.lines().map(str::trim_end);
    lines.any(|l| l == "# /// script") && lines.any(|l| l == "# ///")
}

/// The interpreter of the environment `uv` keeps for `script` (PEP 723),
/// once it has made one (`uv sync --script`): `uv python find --script`
/// names it — or, before then, only a base Python, which is no answer.
fn script_env(uv: &Path, script: &Path) -> Option<PathBuf> {
    let mut cmd = crate::spawn::command(uv);
    cmd.args(["python", "find", "--script"]).arg(script);
    neutral(&mut cmd);
    let out = store::run_bounded(cmd, PROBE_TIMEOUT).ok()?;
    if !out.status.success() {
        return None;
    }
    let found = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    is_script_env(&found).then_some(found)
}

/// Whether `python` is in an environment uv keeps for a script — under its
/// cache's `environments-v…` folder — and not a base Python.
fn is_script_env(python: &Path) -> bool {
    python.is_absolute()
        && python.ancestors().any(|dir| {
            dir.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("environments-v"))
        })
}

/// Install the packages `script` lists into the environment uv keeps for
/// it, then confirm it is found.
fn install_script_env(uv: &Path, script: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let mut cmd = crate::spawn::command(uv);
    cmd.args(["sync", "--script"]).arg(script);
    neutral(&mut cmd);
    let out = store::run_bounded_cancellable(cmd, PIP_TIMEOUT, cancel)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let last = stderr
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("hint:") && !l.starts_with("warning:"))
            .unwrap_or("uv failed");
        return Err(format!(
            "the script's packages could not be installed: {last}"
        ));
    }
    match script_env(uv, script) {
        Some(_) => Ok(()),
        None => {
            let msg =
                "uv installed the script's packages, but its environment wasn't found afterwards"
                    .to_string();
            Err(msg)
        }
    }
}

/// The SDK program the user chose for `language`'s server, when they chose
/// where its toolchain is (a program with no arguments of their own: the
/// toolchain's, run as clew runs it) — the debugger runs the same SDK.
fn chosen_sdk<'a>(choices: &'a clew_protocol::LanguageChoices, language: &str) -> Option<&'a str> {
    choices
        .servers
        .as_ref()?
        .get(language)
        .filter(|choice| choice.args.is_none())
        .map(|choice| choice.command.as_str())
        .filter(|command| !command.trim().is_empty())
}

/// The interpreter to debug a Python program in `root` under: the one the
/// user `chosen`, when it can be used ([`langenv::chosen_interpreter`]); else
/// the project's own virtualenv, exactly as [`langenv`] picks it for the
/// language server — and held to the same check of what executing it would
/// run — else the first `python3` (then `python`) on `path`.
///
/// Every candidate is kept as found, never resolved: a virtualenv's
/// `bin/python` is a symlink to the base interpreter, and Python finds its
/// environment from the path it was run by, so following the link would
/// debug under the base install instead of the venv whose packages the
/// program imports.
fn python_for(
    root: &Path,
    chosen: Option<&str>,
    path: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let chosen = chosen.and_then(|c| langenv::chosen_interpreter(root, c));
    chosen
        .or_else(|| langenv::find_python_interpreter(root))
        .or_else(|| {
            let path = path?;
            store::find_in_unresolved(path, "python3")
                .or_else(|| store::find_in_unresolved(path, "python"))
        })
}

/// lldb-dap for native (Rust/C/C++) binaries. Ships with Xcode / LLVM.
/// The names LLVM's DAP adapter goes by: `lldb-dap`; the versioned
/// `lldb-dap-18` a Linux distribution installs beside other LLVMs, newest
/// first; and `lldb-vscode`, its name before LLVM 18.
fn lldb_dap_names() -> Vec<String> {
    let mut names = vec!["lldb-dap".to_string()];
    names.extend((14..=25).rev().map(|v| format!("lldb-dap-{v}")));
    names.push("lldb-vscode".into());
    names.extend((11..=17).rev().map(|v| format!("lldb-vscode-{v}")));
    names
}

/// A key of a `launch` body that is Clew's, not the adapter's: a notice the
/// client shows in the debug console as the session starts, and takes out
/// of the body before sending it.
pub const NOTICE: &str = "__clewNotice";

/// A key of a `launch` body that is Clew's, not the adapter's: the
/// exception filters the session stops at instead of the adapter's
/// defaults (those of them it offers), taken out before the body is sent.
/// A test run stops where its test fails: debugpy's `userUnhandled`, an
/// exception leaving the project's code for pytest's, which catches every
/// one. Its default (`uncaught`) stopped instead at the `SystemExit` pytest
/// ends with, in pytest's own `__main__`.
pub const EXCEPTIONS: &str = "__clewExceptions";

/// DWARF's language code for Rust (`DW_LANG_Rust`), which rustc puts on
/// its compile units.
const DW_LANG_RUST: u16 = 0x1c;

/// The keys of a `launch` body that only a debugger's run reads — set to
/// stop at what the reader did not ask to stop at (a Rust panic) — and
/// that a run without the debugger leaves out: lldb-dap runs every program
/// under the debugger, `noDebug` or not, and stopped a plain Run at its
/// panic.
pub const DEBUG_ONLY_KEYS: &[&str] = &["preRunCommands"];

/// What lets Rust's summaries show an enum — `Some(7)`, `Circle(1.5)` — in
/// an lldb older than 19: they ask a value for `GetSyntheticValue`, which
/// lldb 18 (a Linux distribution's) has not, and every enum showed a Python
/// error in its place. Given there, it answers no synthetic value, and the
/// summary reads the value itself.
const SYNTHETIC_VALUE_SHIM: &str = "script import lldb; lldb.SBValue.GetSyntheticValue = getattr(lldb.SBValue, 'GetSyntheticValue', None) or (lambda self: lldb.SBValue())";

/// A Rust program debugged as `rust-lldb` sets lldb up for it: the type
/// summaries Rust's toolchain ships (`Vec`, `String`, `HashMap`, `Option`…
/// shown as their values — `size=2`, `"ada"` — not as their fields), read
/// silently, from `sysroot`; and a stop where it panics, at the panic
/// rather than after it, its stack the panic's — what the program did
/// before it went wrong. `quiet`: the adapter runs `?`-prefixed commands
/// without echoing them into the program's output (lldb-dap).
fn rust_setup(launch: &mut serde_json::Value, sysroot: Option<&Path>, quiet: bool) {
    let mark = if quiet { "?" } else { "" };
    let etc = sysroot.map(|s| s.join("lib").join("rustlib").join("etc"));
    let scripts = etc.as_ref().and_then(|etc| {
        let lookup = etc.join("lldb_lookup.py");
        let commands = etc.join("lldb_commands");
        let quoted = |p: &Path| {
            let p = p.to_string_lossy();
            (!p.contains(['"', '\n', '\r'])).then(|| format!("\"{p}\""))
        };
        (lookup.is_file() && commands.is_file()).then(|| {
            Some(vec![
                format!("{mark}{SYNTHETIC_VALUE_SHIM}"),
                format!("{mark}command script import {}", quoted(&lookup)?),
                format!("{mark}command source -s 1 {}", quoted(&commands)?),
            ])
        })?
    });
    if let Some(scripts) = scripts {
        launch["initCommands"] = json!(scripts);
    }
    launch["preRunCommands"] = json!([format!("{mark}breakpoint set --name rust_panic")]);
}

/// A native launch (lldb-dap's, `adapter` the adapter run) for a program
/// known to be Rust's — Cargo built it — set up as [`resolve`] sets up one
/// whose debug information says Rust: Rust's type summaries and the stop at
/// its panic. `resolve` reads that from the ELF file alone; a Mach-O
/// program's DWARF stays in its object files (macOS) and a PE one's in its
/// PDB (Windows), and their runs were left without either. A launch already
/// set up is left as it is.
pub fn set_up_for_rust(launch: &mut serde_json::Value, adapter: &Path, cwd: &Path) {
    if launch.get("initCommands").is_some() || launch.get("preRunCommands").is_some() {
        return;
    }
    let quiet = adapter
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("lldb-dap"));
    rust_setup(launch, rust_sysroot(cwd).as_deref(), quiet);
}

/// The sysroot of the Rust toolchain that builds the program in `cwd` —
/// its `rust-toolchain.toml` honoured, as `rustc` there is rustup's — or
/// `None` when there is no `rustc` or it does not answer in time.
fn rust_sysroot(cwd: &Path) -> Option<PathBuf> {
    let rustc = store::find_in_unresolved(&crate::linerpc::child_path(), "rustc")?;
    let mut command = crate::spawn::command(rustc);
    command
        .args(["--print", "sysroot"])
        .current_dir(cwd)
        .env("PATH", crate::linerpc::child_path());
    let out = store::run_bounded(command, std::time::Duration::from_secs(15)).ok()?;
    let sysroot = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    (out.status.success() && sysroot.is_absolute() && sysroot.is_dir()).then_some(sysroot)
}

/// DWARF's language code for Zig (`DW_LANG_Zig`), which Zig's own code
/// generator — the default for Debug builds on x86-64 — puts on its compile
/// units. lldb has no plugin for it and shows no variables there.
const DW_LANG_ZIG: u16 = 0x27;

/// What a Zig program built by Zig's own code generator is told.
/// Said when a Bun project's script is debugged: under Node, as js-debug
/// speaks V8's inspector protocol and Bun's inspector speaks WebKit's.
const BUN_NOTICE: &str = "This is a Bun project, and Bun's debugger speaks WebKit's inspector protocol, which Clew's JavaScript debugger does not: the file is debugged under Node.js, where Bun's own APIs (Bun.serve, Bun.file) are missing. Run runs it with Bun.";

const ZIG_BACKEND_NOTICE: &str = "This program's debug information comes from Zig's built-in code generator (the default for Debug builds on x86-64), and lldb can't read variables from it. Breakpoints and stepping work, but no variables show. Build it with -fllvm (or .use_llvm = true in build.zig) to see them.";

/// Whether the test file at `path` runs its own tests when run as a script
/// — `unittest.main()`, or pytest's — as one written for `unittest` does.
fn runs_its_own_tests(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .is_ok_and(|text| text.contains("unittest.main(") || text.contains("pytest.main("))
}

/// How long building a Zig program for a session may take.
const ZIG_BUILD_TIMEOUT: Duration = Duration::from_secs(600);

/// `program`, against `cwd` when it is relative.
fn in_dir(cwd: &Path, program: &str) -> PathBuf {
    let program = Path::new(program);
    match program.is_absolute() {
        true => program.to_path_buf(),
        false => cwd.join(program),
    }
}

/// Whether `program` is Zig source, built for the session.
fn is_zig_source(program: &str) -> bool {
    Path::new(program)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zig"))
}

/// The `zig` compiler: the one chosen for the project (Settings →
/// Languages), else the PATH's.
fn zig_for(choices: &clew_protocol::LanguageChoices) -> Result<PathBuf, String> {
    match choices.zig.as_deref().filter(|z| !z.trim().is_empty()) {
        Some(chosen) => store::find_program(chosen)
            .map_err(|why| format!("the Zig toolchain you chose cannot be run: {why}")),
        None => store::find_on_path_unresolved("zig")
            .ok_or_else(|| "zig not found — install Zig to debug a .zig file".to_string()),
    }
}

/// Build `source` for a session — Debug, through LLVM (`-fllvm`), whose
/// debug information lldb reads, unlike that of Zig's own code generator;
/// a Zig built without LLVM builds it its own way. The build and Zig's cache
/// for it go in Clew's data directory, not the project.
fn build_zig(zig: &Path, source: &Path, cwd: &Path) -> Result<PathBuf, String> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    let out = store::data_root()
        .ok_or("no data directory to build the Zig program in")?
        .join("debug-builds")
        .join("zig")
        .join(format!("{:016x}", hasher.finish()));
    std::fs::create_dir_all(&out).map_err(|e| format!("cannot make {}: {e}", out.display()))?;
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "program".into());
    let binary = out.join(match cfg!(windows) {
        true => format!("{stem}.exe"),
        false => stem,
    });
    let build = |llvm: bool| {
        let mut cmd = crate::spawn::command(zig);
        cmd.args(["build-exe", "-O", "Debug"]);
        if llvm {
            cmd.arg("-fllvm");
        }
        cmd.arg("--cache-dir")
            .arg(out.join("cache"))
            .arg(format!("-femit-bin={}", binary.display()))
            .arg(source)
            .current_dir(cwd);
        store::run_bounded(cmd, ZIG_BUILD_TIMEOUT)
    };
    let mut built = build(true)?;
    let said = |o: &std::process::Output| String::from_utf8_lossy(&o.stderr).into_owned();
    if !built.status.success() && said(&built).contains("LLVM") {
        built = build(false)?;
    }
    if !built.status.success() {
        let said = said(&built);
        let tail: Vec<&str> = said.lines().rev().take(40).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(format!(
            "zig could not build {}:\n{}",
            source.display(),
            tail.join("\n")
        ));
    }
    Ok(binary)
}

/// The language DWARF names for an ELF binary's first compile unit
/// (`DW_AT_language`), if it has one this can read: 64-bit little-endian
/// ELF, DWARF 2 to 5, sections not compressed.
fn dwarf_language(binary: &Path) -> Option<u16> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(binary).ok()?;
    let mut head = [0u8; 64];
    file.read_exact(&mut head).ok()?;
    // ELF, 64-bit, little-endian.
    if &head[..4] != b"\x7fELF" || head[4] != 2 || head[5] != 1 {
        return None;
    }
    let u16_at =
        |b: &[u8], at: usize| Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?));
    let u32_at =
        |b: &[u8], at: usize| Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?));
    let u64_at =
        |b: &[u8], at: usize| Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?));
    let shoff = u64_at(&head, 0x28)?;
    let (shentsize, shnum, shstrndx) = (
        usize::from(u16_at(&head, 0x3a)?),
        usize::from(u16_at(&head, 0x3c)?),
        usize::from(u16_at(&head, 0x3e)?),
    );
    if shentsize < 64 || shnum == 0 || shnum > 4096 || shstrndx >= shnum {
        return None;
    }
    let mut table = vec![0u8; shentsize * shnum];
    file.seek(SeekFrom::Start(shoff)).ok()?;
    file.read_exact(&mut table).ok()?;
    // Each section: (name offset, flags, file offset, size).
    let section = |i: usize| {
        let at = i * shentsize;
        Some((
            u32_at(&table, at)? as u64,
            u64_at(&table, at + 8)?,
            u64_at(&table, at + 0x18)?,
            u64_at(&table, at + 0x20)?,
        ))
    };
    let mut read = |offset: u64, size: u64, cap: u64| -> Option<Vec<u8>> {
        let mut bytes = vec![0u8; size.min(cap) as usize];
        file.seek(SeekFrom::Start(offset)).ok()?;
        file.read_exact(&mut bytes).ok()?;
        Some(bytes)
    };
    let (_, _, names_at, names_size) = section(shstrndx)?;
    let names = read(names_at, names_size, 1 << 20)?;
    let mut info = None;
    let mut abbrev = None;
    for i in 0..shnum {
        let (name, flags, offset, size) = section(i)?;
        let name = names.get(name as usize..)?;
        let name = &name[..name.iter().position(|b| *b == 0)?];
        // SHF_COMPRESSED: not read here.
        if flags & 0x800 != 0 {
            continue;
        }
        match name {
            b".debug_info" => info = Some((offset, size)),
            b".debug_abbrev" => abbrev = Some((offset, size)),
            _ => {}
        }
    }
    let (info_at, info_size) = info?;
    let (abbrev_at, abbrev_size) = abbrev?;
    let info = read(info_at, info_size, 1 << 16)?;
    let abbrev = read(abbrev_at, abbrev_size, 16 << 20)?;
    first_unit_language(&info, &abbrev)
}

/// The `DW_AT_language` of the first compile unit in `info` (a
/// `.debug_info`, its start at least), its abbreviations in `abbrev`.
fn first_unit_language(info: &[u8], abbrev: &[u8]) -> Option<u16> {
    fn uleb(b: &[u8], at: &mut usize) -> Option<u64> {
        let (mut value, mut shift) = (0u64, 0u32);
        loop {
            let byte = *b.get(*at)?;
            *at += 1;
            if shift < 64 {
                value |= u64::from(byte & 0x7f) << shift;
            }
            shift += 7;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
    }
    fn sleb(b: &[u8], at: &mut usize) -> Option<i64> {
        let (mut value, mut shift) = (0i64, 0u32);
        loop {
            let byte = *b.get(*at)?;
            *at += 1;
            if shift < 64 {
                value |= i64::from(byte & 0x7f) << shift;
            }
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return Some(value);
            }
        }
    }
    let le = |b: &[u8], at: usize, n: usize| -> Option<u64> {
        Some(
            b.get(at..at + n)?
                .iter()
                .rev()
                .fold(0u64, |v, x| (v << 8) | u64::from(*x)),
        )
    };
    let mut at = 0;
    let mut offset_size = 4;
    if le(info, 0, 4)? == 0xffff_ffff {
        offset_size = 8;
        at = 12;
    } else {
        at += 4;
    }
    let version = le(info, at, 2)?;
    at += 2;
    let (abbrev_offset, address_size) = match version {
        5 => {
            // unit_type, address_size, debug_abbrev_offset
            let address_size = le(info, at + 1, 1)?;
            let offset = le(info, at + 2, offset_size)?;
            at += 2 + offset_size;
            (offset, address_size as usize)
        }
        2..=4 => {
            let offset = le(info, at, offset_size)?;
            let address_size = le(info, at + offset_size, 1)?;
            at += offset_size + 1;
            (offset, address_size as usize)
        }
        _ => return None,
    };
    let code = uleb(info, &mut at)?;
    // The unit's abbreviation: its attributes' names and forms.
    let mut j = usize::try_from(abbrev_offset).ok()?;
    let specs = loop {
        let this = uleb(abbrev, &mut j)?;
        if this == 0 {
            return None;
        }
        uleb(abbrev, &mut j)?; // tag
        j += 1; // children
        let mut specs = Vec::new();
        loop {
            let name = uleb(abbrev, &mut j)?;
            let form = uleb(abbrev, &mut j)?;
            let implicit = match form {
                0x21 => Some(sleb(abbrev, &mut j)?),
                _ => None,
            };
            if name == 0 && form == 0 {
                break;
            }
            specs.push((name, form, implicit));
        }
        if this == code {
            break specs;
        }
    };
    for (name, form, implicit) in specs {
        if name == 0x13 {
            return match form {
                0x0b => le(info, at, 1),
                0x05 => le(info, at, 2),
                0x06 => le(info, at, 4),
                0x0f => uleb(info, &mut at),
                0x21 => implicit.map(|v| v as u64),
                _ => None,
            }
            .and_then(|v| u16::try_from(v).ok());
        }
        let size = match form {
            0x01 => address_size,
            0x0b | 0x0c | 0x11 | 0x25 | 0x29 => 1,
            0x05 | 0x12 | 0x26 | 0x2a => 2,
            0x27 | 0x2b => 3,
            0x06 | 0x13 | 0x1c | 0x28 | 0x2c => 4,
            0x07 | 0x14 | 0x20 | 0x24 => 8,
            0x1e => 16,
            0x0e | 0x10 | 0x17 | 0x1d | 0x1f => offset_size,
            0x19 | 0x21 => 0,
            0x08 => info.get(at..)?.iter().position(|b| *b == 0)? + 1,
            0x0f | 0x15 | 0x1a | 0x1b | 0x22 | 0x23 => {
                uleb(info, &mut at)?;
                0
            }
            0x0d => {
                sleb(info, &mut at)?;
                0
            }
            0x09 | 0x18 => usize::try_from(uleb(info, &mut at)?).ok()?,
            0x0a => {
                let n = usize::from(*info.get(at)?);
                at += 1;
                n
            }
            _ => return None,
        };
        at = at.checked_add(size)?;
    }
    None
}

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

/// Ruby's debugger, `rdbg` (the `debug` gem): it runs the program itself
/// and serves DAP on a loopback TCP port — port 0, announced on stderr as
/// `Debugger can attach via TCP/IP (<addr>)`. Its `launch` runs the program
/// on once configured.
///
/// `--stop-at-load` holds the program as the debugger loads, before its first
/// line. Held AT that line instead (rdbg's default), it wrote the line's
/// listing to a client that had just connected — as text, before reading
/// whether the client speaks DAP — and the session broke on it.
fn ruby_spec(command: PathBuf, program: &str, args: &[String], cwd: &Path) -> AdapterSpec {
    let mut adapter_args: Vec<String> = [
        "--open",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--stop-at-load",
        "--",
    ]
    .map(String::from)
    .to_vec();
    adapter_args.push(program.to_string());
    adapter_args.extend(args.iter().cloned());
    AdapterSpec {
        command,
        args: adapter_args,
        launch: json!({
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
        }),
        transport: AdapterTransport::TcpOnStderr,
    }
}

/// PHP, through clew-server's own adapter (`clew-server php-debug`): it
/// runs the program under `php` with Xdebug and speaks Xdebug's DBGp on one
/// side, DAP over stdio on the other.
fn php_spec(server: &Path, php: &Path, program: &str, args: &[String], cwd: &Path) -> AdapterSpec {
    AdapterSpec {
        command: server.to_path_buf(),
        args: vec![
            "php-debug".into(),
            "--php".into(),
            php.to_string_lossy().into_owned(),
        ],
        launch: json!({
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
        }),
        transport: AdapterTransport::Stdio,
    }
}

/// One of clew-server's own adapters (Erlang's, Haskell's): `server` run with
/// `adapter_args`, speaking DAP over stdio.
fn own_spec(
    server: &Path,
    adapter_args: Vec<String>,
    program: &str,
    args: &[String],
    cwd: &Path,
) -> AdapterSpec {
    AdapterSpec {
        command: server.to_path_buf(),
        args: adapter_args,
        launch: json!({
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
        }),
        transport: AdapterTransport::Stdio,
    }
}

/// netcoredbg for .NET: DAP over stdio (`--interpreter=vscode`); the
/// program is the built assembly (`bin/Debug/<tfm>/App.dll`).
fn csharp_spec(command: PathBuf, program: &str, args: &[String], cwd: &Path) -> AdapterSpec {
    AdapterSpec {
        command,
        args: vec!["--interpreter=vscode".into()],
        launch: json!({
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "stopAtEntry": false,
        }),
        transport: AdapterTransport::Stdio,
    }
}

/// debugpy: its adapter speaks DAP over stdio and debugs the interpreter it
/// runs under — clew's own copy ([`debugpy_adapter`]), run by path, else the
/// interpreter's own `python -m debugpy.adapter`.
fn python_with(
    python: &Path,
    program: &str,
    args: &[String],
    cwd: &Path,
) -> Result<Resolution, String> {
    python_with_adapter(debugpy_adapter(), python, program, args, cwd)
}

/// [`python_with`], with clew's own debugpy (`bundled`, the adapter's
/// directory) given: the tests' seam.
fn python_with_adapter(
    bundled: Option<PathBuf>,
    python: &Path,
    program: &str,
    args: &[String],
    cwd: &Path,
) -> Result<Resolution, String> {
    let probe = probe_python(python)?;
    let adapter_args: Vec<String> = match (bundled, probe.has_debugpy) {
        (Some(dir), _) => vec![dir.to_string_lossy().into_owned()],
        (None, true) => vec!["-m".into(), "debugpy.adapter".into()],
        (None, false) => return Ok(Resolution::NeedsInstall(AdapterInstall::Debugpy)),
    };
    let mut launch = json!({
        "request": "launch",
        "program": program,
        "args": args,
        "cwd": cwd.to_string_lossy(),
        "console": "internalConsole",
        "python": [python.to_string_lossy()],
        "stopOnEntry": false,
        "justMyCode": false,
        // A run to read the code by leaves no trace in it: Python's
        // bytecode cache wrote `__pycache__/*.pyc` beside every module run,
        // listed as uncommitted changes in the reader's own repository.
        "env": { "PYTHONDONTWRITEBYTECODE": "1" },
    });
    // A module of a package (`teashop/pricing.py`) run as a script could
    // not import its own package — "No module named 'teashop'": the folder
    // the package sits in goes on the import path. One that imports its
    // siblings relatively (`from .models import …`) has no package at all
    // as a script, so it runs as Python runs a package's module: `-m`.
    let package = python_package(&in_dir(cwd, program), cwd);
    let mut path: Vec<PathBuf> = package.iter().map(|p| p.root.clone()).collect();
    // A `src/` layout not installed (`src/library/…`, the tests beside it):
    // its tests and programs import the package by name, and pytest run
    // from the project's folder could not ("No module named 'library'").
    // Its `src` goes on the path, as pytest's `pythonpath = ["src"]` puts it.
    if let Some(src) = src_layout(cwd).filter(|src| !path.contains(src)) {
        path.push(src);
    }
    if !path.is_empty() {
        path.extend(
            std::env::var_os("PYTHONPATH")
                .map_or_else(Vec::new, |p| std::env::split_paths(&p).collect::<Vec<_>>()),
        );
        if let Ok(joined) = std::env::join_paths(path) {
            launch["env"]["PYTHONPATH"] = json!(joined.to_string_lossy());
        }
    }
    if let Some(package) = package
        && package.relative_imports
        && let Some(launch) = launch.as_object_mut()
    {
        launch.remove("program");
        launch.insert("module".into(), json!(package.module));
    }
    // One of a Django project's tests, by Django's runner (`manage.py
    // test <label>`): debugged as a test is, stopping where it fails —
    // unittest catches the failure, past the project's own code — and with
    // Django's templates debugged too.
    let django_test = Path::new(program)
        .file_name()
        .is_some_and(|n| n == "manage.py")
        && args.first().is_some_and(|a| a == "test");
    if django_test && let Some(launch) = launch.as_object_mut() {
        launch.insert("justMyCode".into(), json!(true));
        launch.insert("django".into(), json!(true));
        launch.insert(EXCEPTIONS.into(), json!(["userUnhandled"]));
    }
    // A test file is run as its tests are: under pytest, which collects
    // and runs them. Run as a script it defined its tests and ended, and
    // Run picked the project's program instead, saying the test "does not
    // run on its own". One of its tests, named by pytest's node id
    // (`<file>::<Class>::<test>`), is run alone, in any file.
    let one_test = args
        .first()
        .is_some_and(|first| names_a_test_of(first, program));
    if (one_test || is_python_test(Path::new(program)))
        && let Some(launch) = launch.as_object_mut()
    {
        if probe.has_pytest {
            launch.remove("program");
            launch.insert("module".into(), json!("pytest"));
            // Only the project's code is "the user's": an exception leaving
            // it for pytest's is the failing test, and stepping stays out
            // of pytest's machinery.
            launch.insert("justMyCode".into(), json!(true));
            launch.insert(EXCEPTIONS.into(), json!(["userUnhandled"]));
            // pytest's cache (`.pytest_cache`) is not written into the
            // project: a run to read the code by leaves no trace in it, as
            // its bytecode is not written either.
            let mut test_args = vec![json!("-p"), json!("no:cacheprovider")];
            if !one_test {
                test_args.push(json!(program));
            }
            test_args.extend(args.iter().map(|a| json!(a)));
            launch.insert("args".into(), json!(test_args));
        } else if one_test || !runs_its_own_tests(&in_dir(cwd, program)) {
            // Without pytest a test file runs as a script, which only
            // defines its tests, and one test given by its node id ran the
            // whole file as a script with the id as its argument: either
            // ended "passed" having run no test. pytest is asked for, as
            // debugpy is. One that runs its own tests as a script
            // (`unittest.main()`) runs as it is.
            return pytest_install(python, &probe, crate::linerpc::spawn_path().as_deref());
        }
    }
    Ok(Resolution::Ready(AdapterSpec {
        command: python.to_path_buf(),
        args: adapter_args,
        launch,
        transport: AdapterTransport::Stdio,
    }))
}

/// How pytest gets into `python`'s environment, for the consent prompt —
/// or why it cannot, said before anything is asked: pip refuses a system
/// Python its distribution manages (PEP 668), and an environment uv made
/// has no pip, where uv itself installs (`path`, where uv is looked for).
fn pytest_install(
    python: &Path,
    probe: &PythonProbe,
    path: Option<&std::ffi::OsStr>,
) -> Result<Resolution, String> {
    let ask = |via| {
        Ok(Resolution::NeedsInstall(AdapterInstall::Pytest {
            python: python.to_path_buf(),
            via,
        }))
    };
    if probe.managed {
        return Err(format!(
            "pytest isn't installed, and {} is managed by the system, which won't \
             take packages (PEP 668). Create a virtual environment for the project \
             (python3 -m venv .venv): Clew uses it from then on",
            python.display()
        ));
    }
    if probe.has_pip {
        return ask(PyInstaller::Pip {
            user: !probe.in_venv && !probe.conda,
        });
    }
    match path.and_then(|path| store::find_in_unresolved(path, "uv")) {
        Some(uv) => ask(PyInstaller::Uv(uv)),
        None => Err(format!(
            "pytest isn't installed, and {} has no pip to install it with. \
             Install pytest into it (uv add --dev pytest, or python -m ensurepip first)",
            python.display()
        )),
    }
}

/// Whether `arg` is pytest's node id of a test in `program`: the file as
/// given, then `::` and the test's classes and name.
fn names_a_test_of(arg: &str, program: &str) -> bool {
    let normal = |p: &str| p.replace('\\', "/");
    normal(arg)
        .strip_prefix(&normal(program))
        .is_some_and(|rest| rest.starts_with("::") && rest.len() > 2)
}

/// Whether `file` is a pytest test file by its name, as pytest finds them
/// by default: `test_*.py` or `*_test.py`.
pub fn is_python_test(file: &Path) -> bool {
    let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
}

/// The package a Python file is a module of: the folder the outermost
/// package sits in (what goes on the import path), the module's dotted
/// name, and whether it imports relatively. `None` for a file outside any
/// package (no `__init__.py` beside it, and no relative import making a
/// namespace package's module of it), or a package's own `__init__.py`.
#[derive(Debug, PartialEq)]
struct PythonPackage {
    root: PathBuf,
    module: String,
    relative_imports: bool,
}

fn python_package(file: &Path, cwd: &Path) -> Option<PythonPackage> {
    let stem = file.file_stem()?.to_str()?;
    if stem == "__init__" || file.extension()? != "py" {
        return None;
    }
    let source = std::fs::read_to_string(file).unwrap_or_default();
    let relative_imports = source.lines().any(|line| {
        let line = line.trim_start();
        line.strip_prefix("from").is_some_and(|rest| {
            rest.trim_start().starts_with('.') && rest.starts_with(char::is_whitespace)
        })
    });
    let mut dir = file.parent()?;
    let mut parts = Vec::new();
    while dir.join("__init__.py").is_file() {
        parts.push(dir.file_name()?.to_str()?.to_string());
        dir = dir.parent()?;
    }
    if parts.is_empty() {
        // A namespace package's module (PEP 420: no `__init__.py`) that
        // imports its siblings relatively is a package's all the same:
        // named from the folder it is imported from — the project's `src`
        // when it is in one, else the folder it runs in.
        return relative_imports
            .then(|| namespace_module(file, cwd))
            .flatten();
    }
    parts.reverse();
    // `python -m pkg` runs `pkg/__main__.py`.
    if stem != "__main__" {
        parts.push(stem.to_string());
    }
    Some(PythonPackage {
        root: dir.to_path_buf(),
        module: parts.join("."),
        relative_imports,
    })
}

/// [`python_package`] of a namespace package's `file`: its module named
/// from the project's `src`, else `cwd` — every folder on the way a name
/// Python can import — or none outside them.
fn namespace_module(file: &Path, cwd: &Path) -> Option<PythonPackage> {
    let src = cwd.join("src");
    let root = [src, cwd.to_path_buf()]
        .into_iter()
        .find(|root| file.starts_with(root))?;
    let rel = file.strip_prefix(&root).ok()?.with_extension("");
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_str().map(str::to_string))
        .collect::<Option<_>>()?;
    let importable = |p: &String| {
        p.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && p.chars().all(|c| c.is_alphanumeric() || c == '_')
    };
    // A module of no package at all is no package's.
    if parts.len() < 2 || !parts.iter().all(importable) {
        return None;
    }
    let mut parts = parts;
    if parts.last().is_some_and(|p| p == "__main__") {
        parts.pop();
    }
    Some(PythonPackage {
        root,
        module: parts.join("."),
        relative_imports: true,
    })
}

/// The project's `src` folder, when it holds a package (`src/library/
/// __init__.py`): the `src` layout, whose package is imported by name
/// from beside it once installed — or once `src` is on the path.
fn src_layout(root: &Path) -> Option<PathBuf> {
    let src = root.join("src");
    std::fs::read_dir(&src)
        .ok()?
        .flatten()
        .take(MAX_SRC_ENTRIES)
        .any(|entry| entry.path().join("__init__.py").is_file())
        .then_some(src)
}

/// How many of `src`'s entries are looked at for a package: a `src` of
/// thousands is no Python layout, and the look happens before each run.
const MAX_SRC_ENTRIES: usize = 512;

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
/// <addr>`. `tags` is the `-tags=…` flag of the build tags the user chose
/// for the project, so the program is built as the editor reads it.
fn go_spec(
    command: PathBuf,
    program: &str,
    args: &[String],
    cwd: &Path,
    tags: Option<&str>,
) -> AdapterSpec {
    let mut launch = json!({
        "request": "launch",
        "mode": "debug",
        "program": program,
        "args": args,
        "cwd": cwd.to_string_lossy(),
        // The program's output as DAP output, not on delve's own stdout —
        // which no one reads (and, remotely, which is on another host).
        "outputMode": "remote",
    });
    if let Some(tags) = tags {
        launch["buildFlags"] = json!(tags);
    }
    AdapterSpec {
        command,
        args: vec!["dap".into(), "--listen".into(), "127.0.0.1:0".into()],
        launch,
        transport: AdapterTransport::Tcp,
    }
}

/// vscode-js-debug for JS/TS: `node dapDebugServer.js <port> <host>` listens
/// on TCP — port 0, announced as `Debug server listening at <addr>`. It is
/// provisioned under clew's data dir; until it is, this is a
/// [`Resolution::NeedsInstall`].
///
/// A program that is a JavaScript or TypeScript file runs under Node — a
/// TypeScript one with its types stripped, as Run runs it; any other (a
/// test runner the project installed, `node_modules/.bin/vitest`) is run
/// as it is, js-debug attaching to the Node processes it starts.
/// A Deno project's script under vscode-js-debug: Deno runs it with its V8
/// inspector waiting on a free local port (`--inspect-wait`), as Deno's
/// own guide has VS Code do it, and js-debug attaches there.
fn deno_with(node: PathBuf, deno: &Path, program: &str, args: &[String], cwd: &Path) -> Resolution {
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(9229);
    let Resolution::Ready(mut spec) = node_with(node, program, args, cwd) else {
        return Resolution::NeedsInstall(AdapterInstall::JsDebug);
    };
    spec.launch = json!({
        "type": "pwa-node",
        "request": "launch",
        "program": program,
        "args": args,
        "cwd": cwd.to_string_lossy(),
        "runtimeExecutable": deno.to_string_lossy(),
        "runtimeArgs": ["run", format!("--inspect-wait=127.0.0.1:{port}"), "--allow-all"],
        "attachSimplePort": port,
        "console": "internalConsole",
    });
    Resolution::Ready(spec)
}

/// A page debugged in a browser under vscode-js-debug: `url` opened in
/// Chrome (a launch file's `"type": "msedge"` opens Edge, and its own
/// fields — `webRoot`, `runtimeExecutable`, `userDataDir`… — are its), the
/// project's folder its web root, so the source maps the dev server sends
/// lead breakpoints in its `.tsx` and `.vue` files to the code the page
/// runs.
fn browser_with(node: PathBuf, url: &str, cwd: &Path) -> Resolution {
    let Resolution::Ready(mut spec) = node_with(node, url, &[], cwd) else {
        return Resolution::NeedsInstall(AdapterInstall::JsDebug);
    };
    spec.launch = json!({
        "type": "pwa-chrome",
        "request": "launch",
        "url": url,
        "webRoot": cwd.to_string_lossy(),
    });
    Resolution::Ready(spec)
}

fn node_with(node: PathBuf, program: &str, args: &[String], cwd: &Path) -> Resolution {
    let Some(server) = js_debug_server() else {
        return Resolution::NeedsInstall(AdapterInstall::JsDebug);
    };
    let ext = Path::new(program)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let launch = match ext.as_str() {
        "js" | "mjs" | "cjs" | "jsx" => json!({
            "type": "pwa-node",
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "console": "internalConsole",
        }),
        "ts" | "mts" | "cts" | "tsx" => json!({
            "type": "pwa-node",
            "request": "launch",
            "program": program,
            "args": args,
            "runtimeArgs": ["--experimental-strip-types"],
            "cwd": cwd.to_string_lossy(),
            "console": "internalConsole",
        }),
        _ => json!({
            "type": "pwa-node",
            "request": "launch",
            "runtimeExecutable": program,
            "runtimeArgs": args,
            "cwd": cwd.to_string_lossy(),
            "console": "internalConsole",
            "autoAttachChildProcesses": true,
        }),
    };
    Resolution::Ready(AdapterSpec {
        command: node,
        args: vec![
            server.to_string_lossy().into_owned(),
            "0".into(),
            "127.0.0.1".into(),
        ],
        launch,
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
    /// `pytest` is importable: a test file is run under it.
    has_pytest: bool,
    /// `pip` is importable: an environment uv made has none.
    has_pip: bool,
    /// A system Python its distribution manages (PEP 668's
    /// `EXTERNALLY-MANAGED`, outside any virtualenv): pip refuses it.
    managed: bool,
    /// A conda environment (`conda-meta/` in its prefix): packages go into
    /// it, never `--user`.
    conda: bool,
}

const PROBE: &str = "import sys, os, importlib.util, sysconfig\n\
f = lambda m: int(importlib.util.find_spec(m) is not None)\n\
v = int(sys.prefix != getattr(sys, 'base_prefix', sys.prefix))\n\
m = int(not v and os.path.isfile(os.path.join(sysconfig.get_path('stdlib'), 'EXTERNALLY-MANAGED')))\n\
print(f('debugpy'), v, f('pytest'), f('pip'), m, int(os.path.isdir(os.path.join(sys.prefix, 'conda-meta'))))";

/// Ask `python` about itself, time-boxed.
fn probe_python(python: &Path) -> Result<PythonProbe, String> {
    probe_python_within(python, PROBE_TIMEOUT)
}

fn probe_python_within(python: &Path, limit: Duration) -> Result<PythonProbe, String> {
    let mut cmd = crate::spawn::command(python);
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
            has_pytest: fields.next() == Some("1"),
            has_pip: fields.next() != Some("0"),
            managed: fields.next() == Some("1"),
            conda: fields.next() == Some("1"),
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

/// The program and arguments that install pytest, pinned, `via` pip or uv:
/// rendered into the consent prompt as they are run.
fn pytest_argv(python: &Path, via: &PyInstaller) -> (PathBuf, Vec<String>) {
    let pinned = format!("pytest=={PYTEST_VERSION}");
    match via {
        PyInstaller::Pip { user } => {
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
            if *user {
                args.push("--user".into());
            }
            args.push(pinned);
            (python.to_path_buf(), args)
        }
        PyInstaller::Uv(uv) => (
            uv.clone(),
            vec![
                "pip".into(),
                "install".into(),
                "--no-progress".into(),
                "--python".into(),
                python.to_string_lossy().into_owned(),
                pinned,
            ],
        ),
    }
}

fn pytest_command(python: &Path, via: &PyInstaller) -> Command {
    let (program, args) = pytest_argv(python, via);
    let mut cmd = crate::spawn::command(&program);
    cmd.args(args);
    neutral(&mut cmd);
    cmd
}

/// `pm install` in `dir`, with the tools' `PATH` (the Node a package
/// manager runs on): a project's dependencies, as its package manager
/// installs them.
fn install_js_dependencies(pm: &Path, dir: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let mut cmd = crate::spawn::command(pm);
    cmd.arg("install").current_dir(dir);
    if let Some(path) = crate::linerpc::spawn_path() {
        cmd.env("PATH", path);
    }
    let out = store::run_bounded_cancellable(cmd, PIP_TIMEOUT, cancel)?;
    if out.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&out.stderr);
    let last = said
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("it ended with an error");
    let name = pm.file_stem().map_or_else(
        || pm.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    Err(format!("{name} install failed: {last}"))
}

/// The install of `file`'s project's dependencies, when they are not
/// there ([`crate::jsrun::missing_dependencies`]) and its package manager
/// is found.
pub fn js_dependencies(root: &Path, file: &Path) -> Option<AdapterInstall> {
    let dir = crate::jsrun::missing_dependencies(root, file)?;
    let name = crate::jsrun::package_manager(root, &dir);
    let pm = store::find_on_path_unresolved(name)?;
    Some(AdapterInstall::JsDependencies { pm, dir })
}

fn install_pytest(python: &Path, via: &PyInstaller, cancel: &AtomicBool) -> Result<(), String> {
    let out = store::run_bounded_cancellable(pytest_command(python, via), PIP_TIMEOUT, cancel)?;
    if !out.status.success() {
        return Err(install_failure(&String::from_utf8_lossy(&out.stderr)));
    }
    // Confirm with the interpreter the session will actually run under.
    if probe_python(python)?.has_pytest {
        Ok(())
    } else {
        Err(format!(
            "installed pytest but {} still can't import it",
            python.display()
        ))
    }
}

/// What a failed install says, in a sentence: pip's last line is often a
/// hint ("See PEP 668 for the detailed specification.") that names nothing.
fn install_failure(stderr: &str) -> String {
    if stderr.contains("externally-managed-environment") {
        let msg = "this Python is managed by the system and won't take packages (PEP 668). Create a virtual environment for the project (python3 -m venv .venv): Clew uses it from then on".to_string();
        return msg;
    }
    if stderr.contains("No module named pip") {
        let msg =
            "this Python has no pip. Install pytest into it another way (uv add --dev pytest)"
                .to_string();
        return msg;
    }
    let last = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("hint:") && !l.starts_with("note:"))
        .unwrap_or("the install failed");
    format!("pytest=={PYTEST_VERSION} could not be installed: {last}")
}

/// Root under clew's data dir where downloadable debug adapters live.
fn adapters_root() -> Option<PathBuf> {
    Some(store::data_root()?.join("debug-adapters"))
}

/// The versioned install directory for clew's own debugpy.
fn debugpy_dir() -> Option<PathBuf> {
    Some(adapters_root()?.join("debugpy").join(DEBUGPY_VERSION))
}

/// The directory of clew's own debugpy adapter, if downloaded: run by path
/// (`python <dir>`), it puts its own package on the import path, never the
/// project's folder.
fn debugpy_adapter() -> Option<PathBuf> {
    let dir = debugpy_dir()?;
    dir.join(DEBUGPY.binary)
        .is_file()
        .then(|| dir.join("debugpy").join("adapter"))
}

/// Download, verify (pinned SHA-256), and unpack debugpy. Blocking.
fn install_debugpy(cancel: &AtomicBool) -> Result<PathBuf, String> {
    let root = adapters_root().ok_or("no data directory")?;
    store::create_store_dir(&root)?;
    let dest = debugpy_dir().ok_or("no data directory")?;
    store::install_download(&DEBUGPY, &dest, &root, cancel)
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
    store::create_store_dir(&root)?;
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
    let mut cmd = crate::spawn::command(XCRUN);
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
/// over the protocol, whose process channel carries a process's stdio:
///   - stdio adapters (lldb-dap, debugpy, `dart debug_adapter`, netcoredbg)
///     run as they are;
///   - an adapter that listens on TCP (delve, rdbg) listens on the REMOTE's
///     loopback, which the client cannot reach: it runs behind `bridge` —
///     clew-server's own `dap-bridge`, which serves it over stdio — and is
///     refused without one;
///   - Node runs under clew-server's own adapter (`node-debug`, over Node's
///     inspector), not vscode-js-debug, which opens a connection of its own
///     per debugged process — more than one stdio channel carries;
///   - no provisioning: installing on a remote host without the user's
///     explicit consent would break the rule `LspInstall` exists for. The
///     error names what to install instead.
///
/// `root` is the project root, which is also the session's working directory:
/// `SpawnAdapter` carries no other. `choices` are the user's for the project
/// (see [`resolve`]).
pub fn resolve_stdio(
    lang: &str,
    program: &str,
    args: &[String],
    root: &Path,
    choices: &clew_protocol::LanguageChoices,
    bridge: Option<&Path>,
) -> Result<StdioAdapter, String> {
    resolve_stdio_in(lang, program, args, root, root, choices, bridge)
}

/// [`resolve_stdio`] for a program run in `cwd`, a folder of the project
/// at `root`: a Rust test's package, as `cargo test` runs it there.
pub fn resolve_stdio_in(
    lang: &str,
    program: &str,
    args: &[String],
    root: &Path,
    cwd: &Path,
    choices: &clew_protocol::LanguageChoices,
    bridge: Option<&Path>,
) -> Result<StdioAdapter, String> {
    let tcp_refusal = || {
        format!(
            "the {lang} debug adapter speaks DAP over a TCP port on this host, \
             which a remote client cannot reach"
        )
    };
    if lang == "node" {
        let server = bridge.ok_or_else(|| {
            "Node is debugged on a remote host by clew-server's own adapter, \
             and there is none to run"
                .to_string()
        })?;
        let node = store::find_on_path_unresolved("node")
            .ok_or("on the remote host: node not found — install Node.js")?;
        let spec = own_spec(
            server,
            vec![
                "node-debug".into(),
                "--node".into(),
                node.to_string_lossy().into_owned(),
            ],
            program,
            args,
            root,
        );
        return Ok(StdioAdapter {
            command: spec.command,
            args: spec.args,
            launch: spec.launch.to_string(),
        });
    }
    match resolve(lang, program, args, root, cwd, choices, bridge)
        .map_err(|e| format!("on the remote host: {e}"))?
    {
        Resolution::Ready(spec) => match (spec.transport, bridge) {
            (AdapterTransport::Stdio, _) => Ok(StdioAdapter {
                command: spec.command,
                args: spec.args,
                launch: spec.launch.to_string(),
            }),
            (transport, Some(bridge)) => Ok(StdioAdapter {
                command: bridge.to_path_buf(),
                args: bridge_args(transport, &spec.command, &spec.args),
                launch: spec.launch.to_string(),
            }),
            (_, None) => Err(tcp_refusal()),
        },
        Resolution::NeedsInstall(AdapterInstall::Debugpy) => Err(
            "debugpy is not installed on the remote host. Install it there \
             (python3 -m pip install debugpy, or uv pip install debugpy)"
                .into(),
        ),
        Resolution::NeedsInstall(AdapterInstall::ScriptEnv { script, .. }) => Err(format!(
            "the packages {} lists are not installed on the remote host. Install them there: uv sync --script {}",
            script.display(),
            script.display()
        )),
        Resolution::NeedsInstall(install) => Err(format!(
            "{} is not installed on the remote host. Install it there: {}",
            install.name(),
            install.describe()
        )),
    }
}

/// The arguments clew-server's `dap-bridge` runs a TCP adapter with:
/// `dap-bridge --announce stdout|stderr -- <adapter> <args…>` — where it
/// announces its port saying what the rest of its output is.
pub fn bridge_args(transport: AdapterTransport, command: &Path, args: &[String]) -> Vec<String> {
    let announce = match transport {
        AdapterTransport::TcpOnStderr => "stderr",
        AdapterTransport::Tcp | AdapterTransport::Stdio => "stdout",
    };
    let mut out = vec![
        "dap-bridge".to_string(),
        "--announce".to_string(),
        announce.to_string(),
        "--".to_string(),
        command.to_string_lossy().into_owned(),
    ];
    out.extend(args.iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`super::python_with`] as though clew's own debugpy were not
    /// downloaded: the developer's data directory decides nothing here.
    fn python_with(
        python: &Path,
        program: &str,
        args: &[String],
        cwd: &Path,
    ) -> Result<Resolution, String> {
        python_with_adapter(None, python, program, args, cwd)
    }
    #[cfg(unix)]
    use crate::lsp::store::testing::script;
    use crate::lsp::store::testing::with_store;
    #[cfg(unix)]
    use crate::testutil::EnvVars;
    use crate::testutil::TempDir;

    /// A script that lists the packages it needs (PEP 723) is told apart by
    /// its `# /// script` block, closed; the environment uv keeps for it is
    /// told apart from a base Python; and its install is what is shown.
    #[test]
    fn a_script_that_lists_its_packages_runs_in_its_own_environment() {
        let dir = TempDir::new("dap-pep723");
        let script = dir.path().join("tool.py");
        std::fs::write(
            &script,
            "# /// script\n# dependencies = [\"six\"]\n# ///\nimport six\n",
        )
        .unwrap();
        assert!(lists_its_packages(&script));
        let open = dir.path().join("open.py");
        std::fs::write(&open, "# /// script\n# dependencies = []\nimport os\n").unwrap();
        assert!(!lists_its_packages(&open), "never closed");
        let plain = dir.path().join("plain.py");
        std::fs::write(&plain, "import os\n").unwrap();
        assert!(!lists_its_packages(&plain));
        assert!(is_script_env(Path::new(
            "/home/u/.cache/uv/environments-v2/tool-6e053ea93277214d/bin/python3"
        )));
        assert!(!is_script_env(Path::new("/usr/bin/python3")));
        assert!(!is_script_env(Path::new("environments-v2/x/bin/python3")));
        let install = AdapterInstall::ScriptEnv {
            uv: PathBuf::from("/u/bin/uv"),
            script: script.clone(),
        };
        assert!(install.is_script_packages());
        assert!(!install.is_test_runner() && !install.is_download());
        assert_eq!(
            install.describe(),
            format!("/u/bin/uv sync --script {}", script.display())
        );
    }

    /// One of a Django project's tests, run by its own runner, is debugged
    /// as a test is: stopping where it fails (unittest catches the
    /// failure), stepping only through the project's code, with Django's
    /// templates debugged. A plain `manage.py` command is not a test.
    #[cfg(unix)]
    #[test]
    fn a_django_test_is_debugged_as_a_test() {
        let dir = TempDir::new("dap-django-test");
        let python = fake_python(&dir, "echo '1 1 1'");
        let launch = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            match python_with(&python, "/p/manage.py", &args, Path::new("/p")).unwrap() {
                Resolution::Ready(spec) => spec.launch,
                other => panic!("{other:?}"),
            }
        };
        let test = launch(&["test", "shop.tests.CartTests.test_total", "--noinput"]);
        assert_eq!(test["program"], "/p/manage.py");
        assert_eq!(test["justMyCode"], true);
        assert_eq!(test["django"], true);
        assert_eq!(test[EXCEPTIONS], json!(["userUnhandled"]));
        let serve = launch(&["runserver"]);
        assert!(serve.get(EXCEPTIONS).is_none(), "{serve}");
        assert_eq!(serve["justMyCode"], false);
    }

    /// A namespace package's module (no `__init__.py`) that imports its
    /// siblings relatively runs as `python -m`, named from the project's
    /// `src`, else its folder: as a script it had no package at all. One at
    /// the top, or under a folder Python cannot import, does not.
    #[test]
    fn a_namespace_packages_module_runs_as_a_module() {
        let dir = TempDir::new("dap-namespace-pkg");
        let root = dir.path();
        std::fs::create_dir_all(root.join("shop/orders")).unwrap();
        let module = root.join("shop/orders/totals.py");
        std::fs::write(&module, "from .prices import PRICES\n").unwrap();
        let found = python_package(&module, root).expect("a package's module");
        assert_eq!(found.module, "shop.orders.totals");
        assert_eq!(found.root, root);
        assert!(found.relative_imports);
        std::fs::create_dir_all(root.join("src/lib")).unwrap();
        let in_src = root.join("src/lib/core.py");
        std::fs::write(&in_src, "from . import util\n").unwrap();
        let found = python_package(&in_src, root).unwrap();
        assert_eq!(
            (found.module.as_str(), found.root),
            ("lib.core", root.join("src"))
        );
        // Imports only absolutely: run as the script it is.
        let absolute = root.join("shop/cli.py");
        std::fs::write(&absolute, "import shop.orders\n").unwrap();
        assert_eq!(python_package(&absolute, root), None);
        let top = root.join("main.py");
        std::fs::write(&top, "from . import x\n").unwrap();
        assert_eq!(python_package(&top, root), None);
        std::fs::create_dir_all(root.join("my-tools")).unwrap();
        let dashed = root.join("my-tools/run.py");
        std::fs::write(&dashed, "from . import x\n").unwrap();
        assert_eq!(python_package(&dashed, root), None);
    }

    /// A Rust program is debugged as rust-lldb sets lldb up for it: the
    /// toolchain's type summaries read silently from its sysroot, and a stop
    /// at its panic — a key a run without the debugger leaves out.
    #[test]
    fn a_rust_program_is_debugged_with_rusts_type_summaries_and_its_panic() {
        let dir = TempDir::new("rust-lldb-setup");
        let etc = dir.path().join("lib/rustlib/etc");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("lldb_lookup.py"), "").unwrap();
        std::fs::write(etc.join("lldb_commands"), "").unwrap();
        let mut launch = json!({"program": "/p/target/debug/app"});
        rust_setup(&mut launch, Some(dir.path()), false);
        assert_eq!(
            launch["initCommands"],
            json!([
                SYNTHETIC_VALUE_SHIM,
                format!(
                    "command script import \"{}\"",
                    etc.join("lldb_lookup.py").display()
                ),
                format!(
                    "command source -s 1 \"{}\"",
                    etc.join("lldb_commands").display()
                ),
            ])
        );
        assert_eq!(
            launch["preRunCommands"],
            json!(["breakpoint set --name rust_panic"])
        );
        assert!(DEBUG_ONLY_KEYS.contains(&"preRunCommands"));
        // lldb-dap runs them without echoing them into the output.
        let mut launch = json!({});
        rust_setup(&mut launch, Some(dir.path()), true);
        let quiet = |key: &str| {
            launch[key]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c.as_str().unwrap().starts_with('?'))
        };
        assert!(quiet("initCommands") && quiet("preRunCommands"), "{launch}");
        // A toolchain without the scripts, or none found: the stop alone.
        let mut launch = json!({});
        rust_setup(&mut launch, Some(&dir.path().join("elsewhere")), false);
        assert!(launch.get("initCommands").is_none(), "{launch}");
        assert!(launch.get("preRunCommands").is_some());
        let mut launch = json!({});
        rust_setup(&mut launch, None, false);
        assert!(launch.get("initCommands").is_none());
        // A program Cargo built is Rust's, whatever its file says: set up
        // once, and a launch already set up is left alone.
        let mut launch = json!({"program": "/p/target/debug/app"});
        set_up_for_rust(&mut launch, Path::new("/usr/bin/lldb-dap"), dir.path());
        assert_eq!(
            launch["preRunCommands"],
            json!(["?breakpoint set --name rust_panic"])
        );
        let set_up = launch.clone();
        set_up_for_rust(&mut launch, Path::new("/usr/bin/lldb-dap"), dir.path());
        assert_eq!(launch, set_up);
        // rustc's own DWARF language code.
        let (info, abbrev) = dwarf_unit(5, DW_LANG_RUST, 0x0b);
        let app = dir.path().join("app");
        std::fs::write(&app, elf_with(&info, &abbrev)).unwrap();
        assert_eq!(dwarf_language(&app), Some(0x1c));
    }

    /// A compile unit's DWARF: version 5 or 4, its language as `data1` or
    /// `data2`, after a `strp` producer, before an inline name.
    fn dwarf_unit(version: u16, language: u16, form: u8) -> (Vec<u8>, Vec<u8>) {
        let abbrev = vec![1, 0x11, 1, 0x25, 0x0e, 0x13, form, 0x03, 0x08, 0, 0, 0];
        let mut die = vec![1, 0, 0, 0, 0];
        match form {
            0x0b => die.push(language as u8),
            _ => die.extend_from_slice(&language.to_le_bytes()),
        }
        die.extend_from_slice(b"app.zig\0");
        let mut unit = version.to_le_bytes().to_vec();
        match version {
            5 => unit.extend_from_slice(&[1, 8, 0, 0, 0, 0]),
            _ => unit.extend_from_slice(&[0, 0, 0, 0, 8]),
        }
        unit.extend(die);
        let mut info = (unit.len() as u32).to_le_bytes().to_vec();
        info.extend(unit);
        (info, abbrev)
    }

    /// A 64-bit little-endian ELF file holding `info` and `abbrev` as its
    /// `.debug_info` and `.debug_abbrev`.
    fn elf_with(info: &[u8], abbrev: &[u8]) -> Vec<u8> {
        let names = b"\0.shstrtab\0.debug_info\0.debug_abbrev\0";
        let mut body = Vec::new();
        let names_at = 64;
        body.extend_from_slice(names);
        let info_at = 64 + body.len();
        body.extend_from_slice(info);
        let abbrev_at = 64 + body.len();
        body.extend_from_slice(abbrev);
        let shoff = 64 + body.len();
        let mut file = vec![0u8; 64];
        file[..6].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1]);
        file[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
        file[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        file[0x3c..0x3e].copy_from_slice(&4u16.to_le_bytes());
        file[0x3e..0x40].copy_from_slice(&1u16.to_le_bytes());
        file.extend(body);
        let section = |name: u32, offset: usize, size: usize| {
            let mut header = vec![0u8; 64];
            header[..4].copy_from_slice(&name.to_le_bytes());
            header[0x18..0x20].copy_from_slice(&(offset as u64).to_le_bytes());
            header[0x20..0x28].copy_from_slice(&(size as u64).to_le_bytes());
            header
        };
        file.extend(vec![0u8; 64]);
        file.extend(section(1, names_at, names.len()));
        file.extend(section(11, info_at, info.len()));
        file.extend(section(23, abbrev_at, abbrev.len()));
        file
    }

    /// Zig's own code generator marks its units `DW_LANG_Zig`, LLVM marks
    /// them C99: read from the first unit of an ELF binary, by version and
    /// form, and nothing read from what is not one.
    #[test]
    fn a_binary_s_dwarf_language_is_read() {
        for (version, language, form) in [(5, 0x27, 0x0b), (4, 0x0c, 0x05), (5, 0x1d, 0x05)] {
            let (info, abbrev) = dwarf_unit(version, language, form);
            assert_eq!(
                first_unit_language(&info, &abbrev),
                Some(language),
                "DWARF {version}, form {form:#x}"
            );
        }
        let dir = TempDir::new("dwarf-language");
        let (info, abbrev) = dwarf_unit(5, DW_LANG_ZIG, 0x0b);
        let zig = dir.join("zig-app");
        std::fs::write(&zig, elf_with(&info, &abbrev)).unwrap();
        assert_eq!(dwarf_language(&zig), Some(DW_LANG_ZIG));
        let (info, abbrev) = dwarf_unit(4, 0x0c, 0x05);
        let c = dir.join("c-app");
        std::fs::write(&c, elf_with(&info, &abbrev)).unwrap();
        assert_eq!(dwarf_language(&c), Some(0x0c));
        // Not ELF, cut short, a unit naming an abbreviation that is not there.
        std::fs::write(dir.join("script"), "#!/bin/sh\n").unwrap();
        assert_eq!(dwarf_language(&dir.join("script")), None);
        let whole = elf_with(&info, &abbrev);
        std::fs::write(dir.join("cut"), &whole[..whole.len() - 70]).unwrap();
        assert_eq!(dwarf_language(&dir.join("cut")), None);
        assert_eq!(dwarf_language(&dir.join("missing")), None);
        let (mut info, abbrev) = dwarf_unit(5, 0x27, 0x0b);
        info[12] = 9;
        assert_eq!(first_unit_language(&info, &abbrev), None);
        assert_eq!(first_unit_language(&info[..7], &abbrev), None);
    }

    /// A `.zig` program is built for the session through LLVM — its cache
    /// and binary in Clew's data directory — and debugged as that binary; a
    /// Zig without LLVM builds it its own way, and that binary's debug
    /// information (Zig's own) brings the notice on why no variables show.
    #[test]
    #[cfg(unix)]
    fn a_zig_source_file_is_built_for_the_session() {
        with_store("zig-build", |data| {
            let dir = TempDir::new("debug-zig");
            let bin = dir.join("bin");
            script(&bin.join("lldb-dap"), "#!/bin/sh\n");
            let (info, abbrev) = dwarf_unit(4, 0x0c, 0x05);
            std::fs::write(dir.join("llvm.elf"), elf_with(&info, &abbrev)).unwrap();
            let (info, abbrev) = dwarf_unit(5, DW_LANG_ZIG, 0x0b);
            std::fs::write(dir.join("own.elf"), elf_with(&info, &abbrev)).unwrap();
            let log = dir.join("zig.log");
            script(
                &bin.join("zig"),
                &format!(
                    "#!/bin/sh\necho \"$@\" >> '{log}'\nfor a in \"$@\"; do case \"$a\" in -femit-bin=*) out=\"${{a#-femit-bin=}}\";; esac; done\ncase \"$*\" in *-fllvm*) if [ -f '{dir}/no-llvm' ]; then echo 'error: LLVM has not been enabled' >&2; exit 1; fi; /bin/cp '{dir}/llvm.elf' \"$out\";; *) /bin/cp '{dir}/own.elf' \"$out\";; esac\n",
                    log = log.display(),
                    dir = dir.display()
                ),
            );
            let _env = EnvVars::new().set("PATH", bin.as_os_str());
            std::fs::write(dir.join("app.zig"), "pub fn main() void {}\n").unwrap();
            let resolve_zig = || match resolve(
                "native",
                "app.zig",
                &["x".into()],
                &dir,
                &dir,
                &Default::default(),
                None,
            ) {
                Ok(Resolution::Ready(spec)) => spec,
                other => panic!("{other:?}"),
            };
            let spec = resolve_zig();
            let built = PathBuf::from(spec.launch["program"].as_str().unwrap());
            assert!(
                built.starts_with(data.join("debug-builds/zig")),
                "{built:?}"
            );
            assert_eq!(built.file_name().unwrap(), "app");
            assert!(built.is_file());
            assert_eq!(spec.launch["args"][0], "x");
            assert!(spec.launch.get(NOTICE).is_none(), "LLVM's: variables show");
            let ran = std::fs::read_to_string(&log).unwrap();
            assert!(
                ran.starts_with("build-exe -O Debug -fllvm --cache-dir ")
                    && ran.contains(&format!("-femit-bin={}", built.display()))
                    && ran
                        .trim_end()
                        .ends_with(&*dir.join("app.zig").to_string_lossy()),
                "{ran}"
            );
            assert!(
                !dir.join(".zig-cache").exists(),
                "nothing left in the project"
            );

            // A Zig without LLVM: built its own way, and the notice says why
            // no variables will show.
            std::fs::write(dir.join("no-llvm"), "").unwrap();
            let spec = resolve_zig();
            assert!(
                spec.launch[NOTICE].as_str().unwrap().contains("-fllvm"),
                "{}",
                spec.launch
            );
            // A prebuilt binary of Zig's own: debugged as it is, with the notice.
            let own = dir.join("own.elf").to_string_lossy().into_owned();
            let Ok(Resolution::Ready(spec)) =
                resolve("native", &own, &[], &dir, &dir, &Default::default(), None)
            else {
                panic!("a binary resolves");
            };
            assert_eq!(spec.launch["program"], own.as_str());
            assert!(spec.launch.get(NOTICE).is_some());
            // A build error is the error.
            script(
                &bin.join("zig"),
                "#!/bin/sh\necho 'app.zig:1:1: error: expected type' >&2\nexit 1\n",
            );
            let err = resolve(
                "native",
                "app.zig",
                &[],
                &dir,
                &dir,
                &Default::default(),
                None,
            )
            .unwrap_err();
            assert!(err.contains("expected type"), "{err}");
            // No zig at all.
            let _empty = EnvVars::new().set("PATH", dir.join("none").as_os_str());
            let _lldb = EnvVars::new().set("PATH", bin.as_os_str());
            std::fs::remove_file(bin.join("zig")).unwrap();
            let err = resolve(
                "native",
                "app.zig",
                &[],
                &dir,
                &dir,
                &Default::default(),
                None,
            )
            .unwrap_err();
            assert!(err.contains("zig not found"), "{err}");
        });
    }

    /// On a remote host Node runs under clew-server's own adapter, over its
    /// inspector — not js-debug, whose connection per process no stdio
    /// channel carries; with no clew-server to run it, that is said.
    #[test]
    #[cfg(unix)]
    fn a_remote_node_program_runs_under_clew_servers_own_adapter() {
        let dir = TempDir::new("debug-node-remote");
        let node = script(&dir.join("bin/node"), "#!/bin/sh\n");
        let _env = EnvVars::new().set("PATH", dir.join("bin").as_os_str());
        let server = Path::new("/opt/clew/clew-server");
        let remote = resolve_stdio(
            "node",
            "/proj/main.js",
            &["x".into()],
            Path::new("/proj"),
            &Default::default(),
            Some(server),
        )
        .unwrap();
        assert_eq!(remote.command, server);
        assert_eq!(
            remote.args,
            ["node-debug", "--node", &*node.to_string_lossy()]
        );
        let launch: serde_json::Value = serde_json::from_str(&remote.launch).unwrap();
        assert_eq!(launch["program"], "/proj/main.js");
        assert_eq!(launch["args"][0], "x");
        assert_eq!(launch["cwd"], "/proj");
        let err = resolve_stdio(
            "node",
            "/proj/main.js",
            &[],
            Path::new("/proj"),
            &Default::default(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("clew-server"), "{err}");
        let _empty = EnvVars::new().set("PATH", dir.join("nothing").as_os_str());
        let err = resolve_stdio(
            "node",
            "/proj/main.js",
            &[],
            Path::new("/proj"),
            &Default::default(),
            Some(server),
        )
        .unwrap_err();
        assert!(err.contains("install Node.js"), "{err}");
    }

    #[test]
    fn unknown_adapters_are_refused_with_a_reason() {
        assert!(
            resolve_stdio(
                "cobol",
                "/p",
                &[],
                Path::new("/"),
                &Default::default(),
                None
            )
            .unwrap_err()
            .contains("no debug adapter")
        );
    }

    /// A remote TCP adapter runs behind clew-server's bridge, told where it
    /// announces its port; with no bridge it is refused, saying why.
    #[test]
    #[cfg(unix)]
    fn a_remote_tcp_adapter_runs_behind_the_bridge() {
        let dir = TempDir::new("debug-bridge");
        let bin = dir.join("bin");
        let rdbg = script(&bin.join("rdbg"), "#!/bin/sh\n");
        let dlv = script(&bin.join("dlv"), "#!/bin/sh\n");
        let _env = EnvVars::new().set("PATH", bin.as_os_str());
        let bridge = Path::new("/opt/clew/clew-server");
        let ruby = resolve_stdio(
            "ruby",
            "app.rb",
            &[],
            &dir,
            &Default::default(),
            Some(bridge),
        )
        .unwrap();
        assert_eq!(ruby.command, bridge);
        assert_eq!(
            ruby.args[..5],
            [
                "dap-bridge",
                "--announce",
                "stderr",
                "--",
                &*rdbg.to_string_lossy()
            ]
        );
        assert!(
            ruby.args.ends_with(&["--".into(), "app.rb".into()]),
            "{:?}",
            ruby.args
        );
        let go = resolve_stdio(
            "go",
            "./cmd/app",
            &[],
            &dir,
            &Default::default(),
            Some(bridge),
        )
        .unwrap();
        assert_eq!(
            go.args[..5],
            [
                "dap-bridge",
                "--announce",
                "stdout",
                "--",
                &*dlv.to_string_lossy()
            ]
        );
        let err =
            resolve_stdio("ruby", "app.rb", &[], &dir, &Default::default(), None).unwrap_err();
        assert!(err.contains("TCP"), "{err}");
    }

    /// F3: consent is given to the prompt's text, so every argument the
    /// install runs with must be in it, the pin included: pip's, with
    /// `--user` outside an environment, and uv's for one that has no pip.
    /// debugpy is a download, its URL and digest shown.
    #[test]
    fn the_install_argv_is_what_the_consent_prompt_describes() {
        let python = PathBuf::from("/opt/py/bin/python3");
        for via in [
            PyInstaller::Pip { user: true },
            PyInstaller::Pip { user: false },
            PyInstaller::Uv(PathBuf::from("/home/u/.local/bin/uv")),
        ] {
            let install = AdapterInstall::Pytest {
                python: python.clone(),
                via: via.clone(),
            };
            let describe = install.describe();
            let described: Vec<&str> = describe.split_whitespace().collect();
            let cmd = pytest_command(&python, &via);
            let program = match &via {
                PyInstaller::Pip { .. } => python.clone(),
                PyInstaller::Uv(uv) => uv.clone(),
            };
            assert_eq!(cmd.get_program(), program.as_os_str());
            assert_eq!(described.first().copied(), program.to_str());
            for arg in cmd.get_args() {
                let arg = arg.to_str().unwrap();
                assert!(
                    described.contains(&arg),
                    "`{arg}` runs but is not in the prompt"
                );
            }
            let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
            let pinned = format!("pytest=={PYTEST_VERSION}");
            assert!(args.contains(&pinned.as_str()), "{args:?}");
            assert!(install.is_test_runner() && !install.is_download());
            assert_eq!(
                args.contains(&"--user"),
                via == PyInstaller::Pip { user: true }
            );
            if let PyInstaller::Uv(_) = via {
                assert!(
                    args.windows(2)
                        .any(|w| w == ["--python", "/opt/py/bin/python3"])
                );
            }
            // Never run from the project: `-m pip` imports from the cwd.
            let neutral = if cfg!(unix) {
                PathBuf::from("/")
            } else {
                std::env::temp_dir()
            };
            assert_eq!(cmd.get_current_dir(), Some(neutral.as_path()));
        }
        for (install, download) in [
            (AdapterInstall::Debugpy, &DEBUGPY),
            (AdapterInstall::JsDebug, &JS_DEBUG),
        ] {
            let text = install.describe();
            assert!(
                text.contains(download.url) && text.contains(&download.sha256.to_hex()),
                "{text}"
            );
            assert!(install.is_download() && !install.is_test_runner());
        }
        assert!(
            DEBUGPY
                .url
                .contains(&format!("debugpy-{DEBUGPY_VERSION}-py2.py3-none-any.whl"))
        );
    }

    /// pytest goes in by pip, `--user` only outside any environment (a conda
    /// one included); by uv where the environment has no pip; and not at all
    /// into a system Python its distribution manages, which says why first.
    #[test]
    fn pytest_is_installed_the_way_the_environment_takes_it() {
        let dir = TempDir::new("dap-pytest-via");
        let uv = script(&dir.join("bin/uv"), "exit 0");
        let path = dir.join("bin").into_os_string();
        let python = Path::new("/opt/py/bin/python3");
        let probe = |pip, managed, venv, conda| PythonProbe {
            has_debugpy: true,
            in_venv: venv,
            has_pytest: false,
            has_pip: pip,
            managed,
            conda,
        };
        let via =
            |p: PythonProbe, path: Option<&std::ffi::OsStr>| match pytest_install(python, &p, path)
            {
                Ok(Resolution::NeedsInstall(AdapterInstall::Pytest { via, .. })) => Ok(via),
                Ok(other) => panic!("{other:?}"),
                Err(e) => Err(e),
            };
        assert_eq!(
            via(probe(true, false, false, false), None),
            Ok(PyInstaller::Pip { user: true })
        );
        assert_eq!(
            via(probe(true, false, true, false), None),
            Ok(PyInstaller::Pip { user: false })
        );
        assert_eq!(
            via(probe(true, false, false, true), None),
            Ok(PyInstaller::Pip { user: false })
        );
        assert_eq!(
            via(probe(false, false, true, false), Some(&path)),
            Ok(PyInstaller::Uv(uv))
        );
        let no_pip = via(probe(false, false, true, false), None).unwrap_err();
        assert!(
            no_pip.contains("no pip") && no_pip.contains("uv add --dev pytest"),
            "{no_pip}"
        );
        let managed = via(probe(true, true, false, false), Some(&path)).unwrap_err();
        assert!(
            managed.contains("PEP 668") && managed.contains("venv"),
            "{managed}"
        );
    }

    /// A failed install says what failed in a sentence, not pip's last hint.
    #[test]
    fn a_failed_install_names_the_cause() {
        let pep668 = "error: externally-managed-environment\n\n× This environment is externally managed\n\
                      note: If you believe this is a mistake...\nhint: See PEP 668 for the detailed specification.\n";
        let said = install_failure(pep668);
        assert!(
            said.contains("managed by the system") && !said.contains("hint"),
            "{said}"
        );
        let said = install_failure("/p/.venv/bin/python: No module named pip\n");
        assert!(said.contains("has no pip"), "{said}");
        let said =
            install_failure("ERROR: No matching distribution found for pytest==8.3.5\nhint: x\n");
        assert!(
            said.ends_with("No matching distribution found for pytest==8.3.5"),
            "{said}"
        );
    }

    /// A fake interpreter: answers the probe from `probe_answer` (a shell
    /// expression printing "<has_debugpy> <in_venv>"), and records where it
    /// ran and with what. It runs nothing found on `PATH`: a child's `PATH`
    /// is taken once per process (`linerpc::spawn_path`), and a test that
    /// set it to an empty directory may have been the first to take it.
    #[cfg(unix)]
    fn fake_python(dir: &Path, probe_answer: &str) -> PathBuf {
        let log = dir.join("log");
        script(
            &dir.join("bin/python3"),
            &format!(
                "case \"$1\" in -c|-m) echo \"$(pwd) $PYTHONSAFEPATH $*\" >> '{log}' ;; esac\n\
                 case \"$1\" in\n\
                   -c) {probe_answer} ;;\n\
                   -m) : > '{mark}' ;;\n\
                 esac",
                log = log.display(),
                mark = dir.join("installed").display(),
            ),
        )
    }

    /// A test file runs its tests: under pytest, which is installed, with
    /// the file and the run's arguments its arguments. Run as a script it
    /// defined its tests and ended. Without pytest, pytest is asked for; a
    /// file that is no test, or runs its own tests, runs as it is.
    #[test]
    #[cfg(unix)]
    fn a_test_file_runs_under_pytest() {
        let dir = TempDir::new("dap-py-test");
        let with_pytest = fake_python(&dir, "echo '1 1 1'");
        let launch = |python: &Path, program: &str| {
            let Resolution::Ready(spec) =
                python_with(python, program, &["-q".into()], Path::new("/p")).unwrap()
            else {
                panic!("not ready");
            };
            spec.launch
        };
        let test = launch(&with_pytest, "/p/tests/test_pricing.py");
        assert_eq!(test["module"], "pytest");
        assert!(test.get("program").is_none(), "{test}");
        // It stops where a test fails, not at pytest's exit, and steps only
        // through the project's code.
        assert_eq!(test[EXCEPTIONS], json!(["userUnhandled"]));
        assert_eq!(test["justMyCode"], true);
        // pytest's cache folder is not written into the project.
        assert_eq!(
            test["args"],
            json!(["-p", "no:cacheprovider", "/p/tests/test_pricing.py", "-q"])
        );
        let script = launch(&with_pytest, "/p/main.py");
        assert_eq!(script["program"], "/p/main.py");
        assert!(script.get("module").is_none());
        assert!(script.get(EXCEPTIONS).is_none(), "{script}");
        // One test, by its node id: alone, the file not given again — in a
        // file named as tests are or not.
        let one = |program: &str, node: &str| {
            let Resolution::Ready(spec) =
                python_with(&with_pytest, program, &[node.into()], Path::new("/p")).unwrap()
            else {
                panic!("not ready");
            };
            spec.launch
        };
        let test = one(
            "/p/tests/test_pricing.py",
            "/p/tests/test_pricing.py::TestTotal::test_adds",
        );
        assert_eq!(test["module"], "pytest");
        assert_eq!(
            test["args"],
            json!([
                "-p",
                "no:cacheprovider",
                "/p/tests/test_pricing.py::TestTotal::test_adds"
            ])
        );
        let test = one("/p/checks.py", "/p/checks.py::test_ok");
        assert_eq!(test["module"], "pytest");
        assert_eq!(
            test["args"],
            json!(["-p", "no:cacheprovider", "/p/checks.py::test_ok"])
        );
        assert!(!names_a_test_of("/p/checks.py::", "/p/checks.py"));
        assert!(!names_a_test_of("/p/other.py::test_ok", "/p/checks.py"));
        let dir = TempDir::new("dap-py-no-pytest");
        let without = fake_python(&dir, "echo '1 0 0'");
        // Without pytest a test file, or one test of any file, asks for it:
        // run as a script it ended "passed" having run no test.
        let asks = |program: &str, args: &[String]| match python_with(
            &without,
            program,
            args,
            Path::new("/p"),
        )
        .unwrap()
        {
            Resolution::NeedsInstall(AdapterInstall::Pytest { python, via }) => {
                assert_eq!(python, without);
                assert_eq!(
                    via,
                    PyInstaller::Pip { user: true },
                    "--user outside a virtualenv"
                );
                true
            }
            _ => false,
        };
        assert!(asks("/p/tests/test_pricing.py", &[]));
        assert!(asks("/p/checks.py", &["/p/checks.py::test_ok".into()]));
        assert!(
            !dir.join("installed").exists(),
            "resolving must not install"
        );
        assert!(launch(&without, "/p/main.py").get(NOTICE).is_none());
        // One that runs its own tests as a script runs as it is, but not
        // one of its tests by its node id, which only pytest reads.
        let own = dir.path().join("test_own.py");
        std::fs::write(
            &own,
            "import unittest\nclass T(unittest.TestCase):\n    def test_a(self): pass\nif __name__ == '__main__':\n    unittest.main()\n",
        )
        .unwrap();
        let own = own.to_str().unwrap();
        let test = launch(&without, own);
        assert_eq!(test["program"], own);
        assert!(test.get(NOTICE).is_none(), "{test}");
        assert!(asks(own, &[format!("{own}::T::test_a")]));
        assert!(is_python_test(Path::new("a/b_test.py")));
        assert!(!is_python_test(Path::new("a/testing.py")));
        assert!(!is_python_test(Path::new("a/test_x.txt")));
    }

    /// F3: a missing debugpy is a consent request, not an install: to
    /// download clew's own copy, whatever the interpreter (no pip needed).
    #[test]
    #[cfg(unix)]
    fn a_missing_debugpy_is_a_consent_request() {
        let dir = TempDir::new("dap-py");
        let python = fake_python(&dir, "echo '0 1 0 0'");
        let got = python_with(&python, "/p/main.py", &[], Path::new("/p")).unwrap();
        match got {
            Resolution::NeedsInstall(AdapterInstall::Debugpy) => {}
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

    /// Clew's own debugpy, once downloaded, is the adapter, run by its
    /// path under the program's interpreter, which needs no debugpy.
    #[test]
    #[cfg(unix)]
    fn a_downloaded_debugpy_runs_under_the_programs_interpreter() {
        let dir = TempDir::new("dap-py-own");
        let python = fake_python(&dir, "echo '0 1'");
        let own = dir.join("data/debugpy/adapter");
        let Resolution::Ready(spec) = python_with_adapter(
            Some(own.clone()),
            &python,
            "/p/main.py",
            &[],
            Path::new("/p"),
        )
        .unwrap() else {
            panic!("clew's own debugpy is there");
        };
        assert_eq!(spec.command, python);
        assert_eq!(spec.args, [own.to_string_lossy()]);
        assert_eq!(spec.launch["python"][0], python.to_string_lossy().as_ref());
    }

    /// A module of a package runs with the folder its package sits in on
    /// the import path ("No module named 'teashop'" ran it as a lone
    /// script); one importing its siblings relatively runs as `-m`. A lone
    /// script, or a package's `__init__.py`, runs as it always did.
    #[test]
    #[cfg(unix)]
    fn a_package_module_runs_where_its_package_imports() {
        let dir = TempDir::new("dap-py-pkg");
        let python = fake_python(&dir, "echo '1 0'");
        let root = dir.join("proj");
        std::fs::create_dir_all(root.join("teashop/sub")).unwrap();
        std::fs::write(root.join("teashop/__init__.py"), "").unwrap();
        std::fs::write(root.join("teashop/sub/__init__.py"), "").unwrap();
        std::fs::write(
            root.join("teashop/pricing.py"),
            "from teashop.models import Order\n",
        )
        .unwrap();
        std::fs::write(
            root.join("teashop/sub/deep.py"),
            "from ..models import Order\n",
        )
        .unwrap();
        std::fs::write(root.join("main.py"), "import teashop\n").unwrap();
        let launch = |program: &str| {
            let Resolution::Ready(spec) = python_with(&python, program, &[], &root).unwrap() else {
                panic!("debugpy is present");
            };
            spec.launch
        };
        let absolute = launch("teashop/pricing.py");
        assert_eq!(absolute["program"], "teashop/pricing.py");
        let path = absolute["env"]["PYTHONPATH"].as_str().unwrap();
        assert_eq!(
            std::env::split_paths(path).next().as_deref(),
            Some(root.as_path()),
            "the package's folder first on the path"
        );
        let relative = launch(&root.join("teashop/sub/deep.py").to_string_lossy());
        assert!(relative.get("program").is_none(), "{relative}");
        assert_eq!(relative["module"], "teashop.sub.deep");
        let lone = launch("main.py");
        assert_eq!(lone["program"], "main.py");
        assert!(lone.get("module").is_none());
        assert!(lone["env"].get("PYTHONPATH").is_none(), "{lone}");
        // No run of Clew's leaves `__pycache__` in the reader's repository.
        for launch in [&absolute, &relative, &lone] {
            assert_eq!(launch["env"]["PYTHONDONTWRITEBYTECODE"], "1", "{launch}");
        }
        assert_eq!(
            python_package(&root.join("teashop/__init__.py"), &root),
            None
        );
    }

    /// A `src` layout's tests and programs import its package by name: run
    /// from the project's folder, its `src` is on the path — pytest said
    /// "No module named 'library'" of an uninstalled project's every test.
    /// A `src` of plain modules, no package, is no such layout.
    #[test]
    #[cfg(unix)]
    fn a_src_layouts_package_is_on_the_path() {
        let dir = TempDir::new("dap-py-src");
        let python = fake_python(&dir, "echo '1 0 1'");
        let root = dir.join("proj");
        std::fs::create_dir_all(root.join("src/library/services")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("src/library/__init__.py"), "").unwrap();
        std::fs::write(root.join("src/library/services/__init__.py"), "").unwrap();
        std::fs::write(root.join("src/library/services/fines.py"), "X = 1\n").unwrap();
        std::fs::write(
            root.join("tests/test_fines.py"),
            "from library.services.fines import X\n",
        )
        .unwrap();
        let launch = |program: &str| {
            let Resolution::Ready(spec) = python_with(&python, program, &[], &root).unwrap() else {
                panic!("debugpy and pytest are present");
            };
            spec.launch
        };
        let paths = |launch: &serde_json::Value| -> Vec<PathBuf> {
            launch["env"]["PYTHONPATH"]
                .as_str()
                .map(|p| std::env::split_paths(p).collect())
                .unwrap_or_default()
        };
        let test = launch("tests/test_fines.py");
        assert_eq!(test["module"], "pytest", "{test}");
        assert_eq!(paths(&test).first(), Some(&root.join("src")), "{test}");
        // The package's own module: its folder — `src` — once, not twice.
        let module = launch("src/library/services/fines.py");
        let src = root.join("src");
        assert_eq!(
            paths(&module).iter().filter(|p| **p == src).count(),
            1,
            "{module}"
        );
        // Plain modules in `src`: no layout, nothing added.
        std::fs::remove_dir_all(root.join("src/library")).unwrap();
        std::fs::write(root.join("src/tool.py"), "").unwrap();
        let plain = launch("tests/test_fines.py");
        assert!(plain["env"].get("PYTHONPATH").is_none(), "{plain}");
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
                "if [ -f '{}' ]; then echo '1 0 1'; else echo '1 0 0'; fi",
                mark.display()
            ),
        );
        let install = AdapterInstall::Pytest {
            python: python.clone(),
            via: PyInstaller::Pip { user: true },
        };
        install.install().unwrap();
        assert!(mark.exists());
        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        assert!(
            log.contains(&format!("-m pip install --disable-pip-version-check --no-input --user pytest=={PYTEST_VERSION}")),
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

        let chosen = python_for(&project, None, Some(&search)).unwrap();
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
        assert_eq!(python_for(&bare, None, Some(&search)), Some(elsewhere));
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
            &Default::default(),
            None,
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

    /// F10: a pytest install stops when cancelled, with what it started,
    /// instead of running out its ten-minute limit.
    #[test]
    #[cfg(unix)]
    fn a_cancelled_pytest_install_stops() {
        let dir = TempDir::new("dap-pip-cancel");
        let python = script(
            &dir.join("bin/python3"),
            "case \"$1\" in -m) /bin/sleep 60 ;; *) echo '0 0' ;; esac",
        );
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flip = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        };
        let started = std::time::Instant::now();
        let install = AdapterInstall::Pytest {
            python,
            via: PyInstaller::Pip { user: true },
        };
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
        let python = fake_python(&dir, "/bin/sleep 30");
        let started = std::time::Instant::now();
        let err = probe_python_within(&python, Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("did not finish"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// The project's dependencies installed for real, by Bun when it is
    /// on this computer: `CLEW_LIVE_JS_INSTALL=1` (it reaches the npm
    /// registry for one small package with no install scripts).
    #[test]
    #[ignore = "reaches the npm registry; CLEW_LIVE_JS_INSTALL=1"]
    fn live_js_dependencies_install() {
        if std::env::var_os("CLEW_LIVE_JS_INSTALL").is_none() {
            return;
        }
        let dir = TempDir::new("live-js-deps");
        std::fs::write(
            dir.join("package.json"),
            r#"{"name": "live", "dependencies": {"is-number": "7.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("bun.lock"), "").unwrap();
        let file = dir.join("index.js");
        std::fs::write(&file, "require('is-number')\n").unwrap();
        let install = js_dependencies(&dir, &file).expect("bun is found, the dependencies are not");
        let AdapterInstall::JsDependencies { pm, .. } = &install else {
            panic!("{install:?}");
        };
        assert_eq!(pm.file_stem().unwrap(), "bun");
        println!("{}", install.describe());
        install.install().unwrap();
        assert!(dir.join("node_modules/is-number/package.json").is_file());
        assert_eq!(js_dependencies(&dir, &file), None, "installed now");
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
            assert_eq!(Path::new(&spec.args[0]), entry);
            assert_eq!(&spec.args[1..], ["0", "127.0.0.1"]);
            assert_eq!(spec.launch["program"], "/p/app.js");
            // TypeScript, with its types stripped, as Run runs it.
            let Resolution::Ready(ts) =
                node_with(PathBuf::from("node"), "/p/app.ts", &[], Path::new("/p"))
            else {
                panic!("installed");
            };
            assert_eq!(
                ts.launch["runtimeArgs"],
                json!(["--experimental-strip-types"])
            );
            // A test runner the project installed: run as it is, js-debug
            // attaching to the Node it starts.
            let args = vec!["run".to_string(), "./a.test.ts".to_string()];
            let Resolution::Ready(runner) = node_with(
                PathBuf::from("node"),
                "/p/node_modules/.bin/vitest",
                &args,
                Path::new("/p"),
            ) else {
                panic!("installed");
            };
            assert_eq!(
                runner.launch["runtimeExecutable"],
                "/p/node_modules/.bin/vitest"
            );
            assert_eq!(runner.launch["runtimeArgs"], json!(args));
            assert_eq!(runner.launch["autoAttachChildProcesses"], true);
            assert_eq!(runner.launch.get("program"), None);
            // A Deno project's script under Deno, its inspector waiting on
            // the port js-debug attaches to.
            let Resolution::Ready(deno) = deno_with(
                PathBuf::from("node"),
                Path::new("/opt/deno"),
                "/p/main.ts",
                &[],
                Path::new("/p"),
            ) else {
                panic!("installed");
            };
            assert_eq!(deno.launch["runtimeExecutable"], "/opt/deno");
            let port = deno.launch["attachSimplePort"].as_u64().unwrap();
            assert_eq!(
                deno.launch["runtimeArgs"],
                json!([
                    "run",
                    format!("--inspect-wait=127.0.0.1:{port}"),
                    "--allow-all"
                ])
            );
            assert_eq!(deno.launch["program"], "/p/main.ts");
            // A page in a browser: its URL, the project its web root.
            let Resolution::Ready(page) = browser_with(
                PathBuf::from("node"),
                "http://localhost:5173",
                Path::new("/p"),
            ) else {
                panic!("installed");
            };
            assert_eq!(page.launch["type"], "pwa-chrome");
            assert_eq!(page.launch["url"], "http://localhost:5173");
            assert_eq!(page.launch["webRoot"], "/p");
            assert_eq!(Path::new(&page.args[0]), entry);
        });
        let go = go_spec(PathBuf::from("/bin/dlv"), "/p", &[], Path::new("/p"), None);
        assert_eq!(go.args, ["dap", "--listen", "127.0.0.1:0"]);
        assert_eq!(go.transport, AdapterTransport::Tcp);
        assert_eq!(go.launch.get("buildFlags"), None);
    }

    /// The Go program is built with the tags the user chose for the
    /// project, as gopls reads it — and only tags that are tags.
    #[test]
    fn a_go_program_is_debugged_with_the_chosen_build_tags() {
        let go = clew_protocol::GoChoices {
            tags: vec!["integration".into(), "not a tag".into(), "e2e".into()],
            ..Default::default()
        };
        let flag = langenv::go_tags_flag(&go);
        let spec = go_spec(
            PathBuf::from("/bin/dlv"),
            "/p",
            &[],
            Path::new("/p"),
            flag.as_deref(),
        );
        assert_eq!(spec.launch["buildFlags"], "-tags=integration,e2e");
        assert_eq!(
            spec.launch["outputMode"], "remote",
            "its output comes as DAP output"
        );
    }

    /// A Dart program is debugged with the SDK chosen for its server — when
    /// that choice is where the toolchain is, not a program of its own — and
    /// a chosen SDK that is gone says so rather than falling back.
    #[test]
    #[cfg(unix)]
    fn a_dart_program_is_debugged_with_the_chosen_sdk() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testutil::TempDir::new("debug-dart-sdk");
        let dart = dir.join("flutter/bin/dart");
        std::fs::create_dir_all(dart.parent().unwrap()).unwrap();
        std::fs::write(&dart, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&dart, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sdk = |command: &Path, args: Option<Vec<String>>| clew_protocol::LanguageChoices {
            servers: Some(
                [(
                    "dart".to_string(),
                    clew_protocol::ServerChoice {
                        command: command.to_string_lossy().into_owned(),
                        args,
                    },
                )]
                .into(),
            ),
            ..Default::default()
        };
        let resolved = resolve(
            "dart",
            "/p/main.dart",
            &[],
            &dir,
            &dir,
            &sdk(&dart, None),
            None,
        );
        let Ok(Resolution::Ready(spec)) = resolved else {
            panic!("the chosen SDK is used: {resolved:?}");
        };
        assert_eq!(spec.command, dart);
        assert_eq!(spec.args, ["debug_adapter"]);
        let gone = resolve(
            "dart",
            "/p/main.dart",
            &[],
            &dir,
            &dir,
            &sdk(&dir.join("gone/dart"), None),
            None,
        )
        .unwrap_err();
        assert!(gone.contains("the Dart SDK you chose"), "{gone}");
        // A server program of the user's own (with its own arguments) is
        // not an SDK: the debugger finds Dart as it would otherwise.
        assert_eq!(
            chosen_sdk(&sdk(&dart, Some(vec!["--lsp".into()])), "dart"),
            None
        );
    }

    /// A Ruby program runs under `rdbg`, which serves DAP on a port it picks
    /// and names on stderr: the `rdbg` beside the Ruby LSP the user chose,
    /// else the PATH's. A C# assembly is debugged by netcoredbg over stdio.
    #[test]
    #[cfg(unix)]
    fn ruby_runs_under_rdbg_and_csharp_under_netcoredbg() {
        let dir = TempDir::new("debug-ruby-cs");
        let path_bin = dir.join("path-bin");
        let on_path = script(&path_bin.join("rdbg"), "#!/bin/sh\n");
        script(&path_bin.join("netcoredbg"), "#!/bin/sh\n");
        let chosen_bin = dir.join("rubies/3.3.0/bin");
        script(&chosen_bin.join("ruby-lsp"), "#!/bin/sh\n");
        let beside = script(&chosen_bin.join("rdbg"), "#!/bin/sh\n");
        let _env = EnvVars::new().set("PATH", path_bin.as_os_str());
        let ruby_lsp = |command: &Path| clew_protocol::LanguageChoices {
            servers: Some(
                [(
                    "ruby".to_string(),
                    clew_protocol::ServerChoice {
                        command: command.to_string_lossy().into_owned(),
                        args: None,
                    },
                )]
                .into(),
            ),
            ..Default::default()
        };
        let ready =
            |lang: &str, program: &str, choices: &clew_protocol::LanguageChoices| match resolve(
                lang,
                program,
                &["a".into()],
                &dir,
                &dir,
                choices,
                None,
            ) {
                Ok(Resolution::Ready(spec)) => spec,
                other => panic!("{lang}: {other:?}"),
            };

        let spec = ready("ruby", "app.rb", &Default::default());
        assert_eq!(spec.command, on_path);
        assert_eq!(
            spec.args,
            [
                "--open",
                "--host",
                "127.0.0.1",
                "--port",
                "0",
                "--stop-at-load",
                "--",
                "app.rb",
                "a"
            ]
        );
        assert_eq!(spec.transport, AdapterTransport::TcpOnStderr);
        assert_eq!(spec.launch["request"], "launch");
        let spec = ready("ruby", "app.rb", &ruby_lsp(&chosen_bin.join("ruby-lsp")));
        assert_eq!(spec.command, beside, "the chosen Ruby's own rdbg");
        // A chosen Ruby LSP with no rdbg beside it: the PATH's.
        let spec = ready("ruby", "app.rb", &ruby_lsp(&dir.join("other/ruby-lsp")));
        assert_eq!(spec.command, on_path);

        let spec = ready("csharp", "bin/Debug/net8.0/App.dll", &Default::default());
        assert_eq!(spec.args, ["--interpreter=vscode"]);
        assert_eq!(spec.transport, AdapterTransport::Stdio);
        assert_eq!(spec.launch["program"], "bin/Debug/net8.0/App.dll");
        assert_eq!(spec.launch["stopAtEntry"], false);
        let remote =
            resolve_stdio("csharp", "App.dll", &[], &dir, &Default::default(), None).unwrap();
        assert_eq!(remote.args, ["--interpreter=vscode"]);

        let _none = EnvVars::new().set("PATH", dir.join("empty").as_os_str());
        let err =
            resolve("ruby", "app.rb", &[], &dir, &dir, &Default::default(), None).unwrap_err();
        assert!(err.contains("gem install debug"), "{err}");
        let err = resolve(
            "csharp",
            "App.dll",
            &[],
            &dir,
            &dir,
            &Default::default(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("netcoredbg"), "{err}");
    }

    /// A PHP program is debugged by clew-server's own adapter, given the
    /// PATH's `php`; with no clew-server to run it, that is said.
    #[test]
    #[cfg(unix)]
    fn php_runs_under_clew_servers_own_adapter() {
        let dir = TempDir::new("debug-php");
        let php = script(&dir.join("bin/php"), "#!/bin/sh\n");
        let _env = EnvVars::new().set("PATH", dir.join("bin").as_os_str());
        let server = Path::new("/opt/clew/clew-server");
        let resolved = resolve(
            "php",
            "app.php",
            &["x".into()],
            &dir,
            &dir,
            &Default::default(),
            Some(server),
        );
        let Ok(Resolution::Ready(spec)) = resolved else {
            panic!("{resolved:?}");
        };
        assert_eq!(spec.command, server);
        assert_eq!(spec.args, ["php-debug", "--php", &*php.to_string_lossy()]);
        assert_eq!(spec.transport, AdapterTransport::Stdio);
        assert_eq!(spec.launch["program"], "app.php");
        assert_eq!(spec.launch["args"][0], "x");
        let err =
            resolve("php", "app.php", &[], &dir, &dir, &Default::default(), None).unwrap_err();
        assert!(err.contains("clew-server"), "{err}");
        // Remotely, the server is its own.
        let remote = resolve_stdio(
            "php",
            "app.php",
            &[],
            &dir,
            &Default::default(),
            Some(server),
        )
        .unwrap();
        assert_eq!(remote.args[0], "php-debug");
    }

    /// A Java program is debugged by clew-server's own adapter, with the JDK
    /// chosen for the project (Settings → Language servers), else the PATH's.
    #[test]
    #[cfg(unix)]
    fn java_runs_under_clew_servers_own_adapter_with_the_chosen_jdk() {
        let dir = TempDir::new("debug-java");
        let java = script(&dir.join("bin/java"), "#!/bin/sh\n");
        let javac = script(&dir.join("bin/javac"), "#!/bin/sh\n");
        let jdk = dir.join("jdks/temurin-21");
        let chosen_java = script(&jdk.join("bin/java"), "#!/bin/sh\n");
        let chosen_javac = script(&jdk.join("bin/javac"), "#!/bin/sh\n");
        let _env = EnvVars::new().set("PATH", dir.join("bin").as_os_str());
        let server = Path::new("/opt/clew/clew-server");
        let ready = |choices: &clew_protocol::LanguageChoices| match resolve(
            "java",
            "/p/Main.java",
            &[],
            &dir,
            &dir,
            choices,
            Some(server),
        ) {
            Ok(Resolution::Ready(spec)) => spec,
            other => panic!("{other:?}"),
        };
        let spec = ready(&Default::default());
        assert_eq!(
            spec.args,
            [
                "java-debug",
                "--java",
                &*java.to_string_lossy(),
                "--javac",
                &*javac.to_string_lossy()
            ]
        );
        let chosen = clew_protocol::LanguageChoices {
            java: Some(jdk.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let spec = ready(&chosen);
        assert_eq!(spec.args[2], chosen_java.to_string_lossy());
        assert_eq!(spec.args[4], chosen_javac.to_string_lossy());
        let err = resolve(
            "java",
            "/p/Main.java",
            &[],
            &dir,
            &dir,
            &Default::default(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("clew-server"), "{err}");
    }

    /// Erlang and Haskell programs are debugged by clew-server's own
    /// adapters, given the PATH's `erl`/`erlc` and `ghci` — Haskell's the
    /// `ghci` beside the language server the user chose, when there is one.
    #[test]
    #[cfg(unix)]
    fn erlang_and_haskell_run_under_clew_servers_own_adapters() {
        let dir = TempDir::new("debug-erl-hs");
        let erl = script(&dir.join("bin/erl"), "#!/bin/sh\n");
        let erlc = script(&dir.join("bin/erlc"), "#!/bin/sh\n");
        let ghci = script(&dir.join("bin/ghci"), "#!/bin/sh\n");
        script(
            &dir.join("ghcup/bin/haskell-language-server-wrapper"),
            "#!/bin/sh\n",
        );
        let chosen_ghci = script(&dir.join("ghcup/bin/ghci"), "#!/bin/sh\n");
        let _env = EnvVars::new().set("PATH", dir.join("bin").as_os_str());
        let server = Path::new("/opt/clew/clew-server");
        let ready = |lang: &str, choices: &clew_protocol::LanguageChoices| match resolve(
            lang,
            "/p/main",
            &["a".into()],
            &dir,
            &dir,
            choices,
            Some(server),
        ) {
            Ok(Resolution::Ready(spec)) => spec,
            other => panic!("{lang}: {other:?}"),
        };
        let spec = ready("erlang", &Default::default());
        assert_eq!(spec.command, server);
        assert_eq!(
            spec.args,
            [
                "erlang-debug",
                "--erl",
                &*erl.to_string_lossy(),
                "--erlc",
                &*erlc.to_string_lossy()
            ]
        );
        assert_eq!(spec.launch["args"][0], "a");
        let spec = ready("haskell", &Default::default());
        assert_eq!(
            spec.args,
            ["haskell-debug", "--ghci", &*ghci.to_string_lossy()]
        );
        let chosen = clew_protocol::LanguageChoices {
            servers: Some(
                [(
                    "haskell".to_string(),
                    clew_protocol::ServerChoice {
                        command: dir
                            .join("ghcup/bin/haskell-language-server-wrapper")
                            .to_string_lossy()
                            .into_owned(),
                        args: None,
                    },
                )]
                .into(),
            ),
            ..Default::default()
        };
        let spec = ready("haskell", &chosen);
        assert_eq!(
            spec.args[2],
            chosen_ghci.to_string_lossy(),
            "the chosen GHC's ghci"
        );
        for lang in ["erlang", "haskell"] {
            let err =
                resolve(lang, "/p/main", &[], &dir, &dir, &Default::default(), None).unwrap_err();
            assert!(err.contains("clew-server"), "{err}");
        }
        let _none = EnvVars::new().set("PATH", dir.join("empty").as_os_str());
        let err = resolve(
            "erlang",
            "/p/main",
            &[],
            &dir,
            &dir,
            &Default::default(),
            Some(server),
        )
        .unwrap_err();
        assert!(err.contains("Erlang/OTP"), "{err}");
    }

    /// LLVM's adapter is found by its plain name, a distribution's versioned
    /// one (the newest), or its name before LLVM 18.
    #[test]
    #[cfg(unix)]
    fn lldb_dap_is_found_by_any_of_its_names() {
        let dir = TempDir::new("debug-lldb-names");
        let native = |path: &Path| {
            let _env = EnvVars::new().set("PATH", path.as_os_str());
            match resolve(
                "native",
                "/p/app",
                &[],
                &dir,
                &dir,
                &Default::default(),
                None,
            ) {
                Ok(Resolution::Ready(spec)) => Some(spec.command),
                _ => None,
            }
        };
        let versioned = dir.join("versioned");
        script(&versioned.join("lldb-dap-17"), "#!/bin/sh\n");
        let newest = script(&versioned.join("lldb-dap-18"), "#!/bin/sh\n");
        assert_eq!(native(&versioned), Some(newest));
        let old = dir.join("old");
        let vscode = script(&old.join("lldb-vscode"), "#!/bin/sh\n");
        assert_eq!(native(&old), Some(vscode));
        let plain = dir.join("plain");
        let dap = script(&plain.join("lldb-dap"), "#!/bin/sh\n");
        script(&plain.join("lldb-dap-18"), "#!/bin/sh\n");
        assert_eq!(native(&plain), Some(dap), "the plain name first");
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
