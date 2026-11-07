//! Per-language debug-adapter resolution: maps a program + language to the DAP
//! adapter to spawn and the `launch` request arguments that adapter expects.
//! This is the seam that makes clew's debugger multi-language — the DAP client
//! and UI are identical across languages; only the adapter and the
//! launch-argument shape differ.
//!
//! The resolution itself lives in [`clew_core::debugadapter`], shared with
//! clew-server (which resolves the same way for remote sessions). This module
//! adds what only the client needs: picking the language, and the labels.
//!
//! Resolution never installs anything. A missing adapter that clew can
//! provision (debugpy, vscode-js-debug) comes back as
//! [`Resolved::NeedsInstall`], and the app asks the user — through the same
//! consent modal a language-server install uses — before
//! [`AdapterInstall::install_cancellable`] runs.

use std::path::{Path, PathBuf};

use clew_core::debugadapter::{self, AdapterTransport, Resolution};
use serde_json::Value;

pub use clew_core::debugadapter::AdapterInstall;

use super::client::Transport;
use crate::tr_noop;

/// A resolved adapter ready to spawn, plus the launch request for the program.
pub struct Adapter {
    /// The DAP adapter executable.
    pub command: PathBuf,
    /// Arguments to the adapter process itself (not the debuggee).
    pub args: Vec<String>,
    /// The `launch` request arguments (adapter-specific shape).
    pub launch: Value,
    /// How to talk to the adapter (stdio, or a TCP port it listens on).
    pub transport: Transport,
}

/// The outcome of [`resolve`].
pub enum Resolved {
    Ready(Adapter),
    /// The adapter must be installed first, with the user's consent.
    NeedsInstall(AdapterInstall),
}

/// Languages clew can debug, each backed by a DAP adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// Rust / C / C++ (native) via lldb-dap.
    Native,
    Python,
    Go,
    Dart,
    /// JavaScript / TypeScript via vscode-js-debug.
    Node,
    /// Ruby via rdbg (the `debug` gem).
    Ruby,
    /// C# and the rest of .NET via netcoredbg.
    CSharp,
    /// PHP via Xdebug, through clew-server's own adapter.
    Php,
    /// Erlang via OTP's interpreter, through clew-server's own adapter.
    Erlang,
    /// Java via JDWP, through clew-server's own adapter.
    Java,
    /// Haskell via GHCi's debugger, through clew-server's own adapter.
    Haskell,
}

impl Lang {
    /// Pick the language from an explicit launch.json `type`, else the program's
    /// file extension (no extension ⇒ a compiled native binary).
    pub fn detect(type_hint: Option<&str>, program: &Path) -> Option<Lang> {
        if let Some(t) = type_hint {
            return match t.to_ascii_lowercase().as_str() {
                "rust" | "lldb" | "lldb-dap" | "cpp" | "c" | "codelldb" | "native" | "zig" => {
                    Some(Lang::Native)
                }
                "ruby" | "rdbg" => Some(Lang::Ruby),
                "coreclr" | "netcoredbg" | "csharp" | "cs" | "dotnet" => Some(Lang::CSharp),
                "php" | "xdebug" => Some(Lang::Php),
                "erlang" | "erl" => Some(Lang::Erlang),
                "java" | "jdwp" => Some(Lang::Java),
                "haskell" | "ghc" | "ghci" | "hs" => Some(Lang::Haskell),
                "python" | "debugpy" => Some(Lang::Python),
                "go" | "delve" | "dlv" => Some(Lang::Go),
                "dart" | "flutter" => Some(Lang::Dart),
                "node" | "js" | "ts" | "javascript" | "typescript" | "pwa-node" => Some(Lang::Node),
                _ => None,
            };
        }
        match program
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("py") => Some(Lang::Python),
            Some("dart") => Some(Lang::Dart),
            Some("go") => Some(Lang::Go),
            Some("js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx") => Some(Lang::Node),
            Some("rb") => Some(Lang::Ruby),
            Some("php") => Some(Lang::Php),
            Some("erl") => Some(Lang::Erlang),
            Some("java") => Some(Lang::Java),
            Some("hs" | "lhs") => Some(Lang::Haskell),
            // A .NET assembly: what netcoredbg runs.
            Some("dll") => Some(Lang::CSharp),
            None => Some(Lang::Native),
            _ => Some(Lang::Native),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Lang::Native => tr_noop!("native (lldb)"),
            Lang::Python => "Python (debugpy)",
            Lang::Go => "Go (delve)",
            Lang::Dart => "Dart",
            Lang::Node => "Node (js-debug)",
            Lang::Ruby => "Ruby (rdbg)",
            Lang::CSharp => "C# (netcoredbg)",
            Lang::Php => "PHP (Xdebug)",
            Lang::Erlang => tr_noop!("Erlang (OTP interpreter)"),
            Lang::Java => "Java (JDWP)",
            Lang::Haskell => "Haskell (GHCi)",
        }
    }

    /// The wire slug `SpawnAdapter` carries, and the key the shared resolver
    /// (`clew_core::debugadapter`) matches on.
    pub fn slug(self) -> &'static str {
        match self {
            Lang::Native => "native",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::Dart => "dart",
            Lang::Node => "node",
            Lang::Ruby => "ruby",
            Lang::CSharp => "csharp",
            Lang::Php => "php",
            Lang::Erlang => "erlang",
            Lang::Java => "java",
            Lang::Haskell => "haskell",
        }
    }
}

/// Resolve the adapter + launch args for a program in `lang`. Blocking (it
/// may run `xcrun` or probe a Python interpreter, each time-boxed): call it
/// off the UI thread and off the async executor.
///
/// `root` is the project root, where a Python virtualenv is looked for; `cwd`
/// is the session's working directory from the launch configuration; a
/// Python program runs under the interpreter `choices` names, when it names
/// one (see `clew_core::debugadapter::resolve`).
pub fn resolve(
    lang: Lang,
    program: &Path,
    args: &[String],
    root: &Path,
    cwd: &Path,
    choices: &clew_protocol::LanguageChoices,
) -> Result<Resolved, String> {
    let program = program.to_string_lossy();
    // clew-server serves clew's own adapters (PHP's).
    let server = crate::backend::server::server_bin_path();
    Ok(
        match debugadapter::resolve(
            lang.slug(),
            &program,
            args,
            root,
            cwd,
            choices,
            server.as_deref(),
        )? {
            Resolution::Ready(spec) => Resolved::Ready(Adapter {
                command: spec.command,
                args: spec.args,
                launch: spec.launch,
                transport: match spec.transport {
                    AdapterTransport::Stdio => Transport::Stdio,
                    AdapterTransport::Tcp => Transport::Tcp,
                    AdapterTransport::TcpOnStderr => Transport::TcpOnStderr,
                },
            }),
            Resolution::NeedsInstall(install) => Resolved::NeedsInstall(install),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_language_by_type_then_extension() {
        assert_eq!(
            Lang::detect(Some("python"), Path::new("x")),
            Some(Lang::Python)
        );
        assert_eq!(Lang::detect(Some("go"), Path::new("x")), Some(Lang::Go));
        assert_eq!(
            Lang::detect(None, Path::new("a/main.py")),
            Some(Lang::Python)
        );
        assert_eq!(
            Lang::detect(None, Path::new("a/main.dart")),
            Some(Lang::Dart)
        );
        assert_eq!(Lang::detect(None, Path::new("a/app.ts")), Some(Lang::Node));
        // No extension ⇒ a compiled native binary.
        assert_eq!(
            Lang::detect(None, Path::new("target/debug/app")),
            Some(Lang::Native)
        );
        assert_eq!(
            Lang::detect(None, Path::new("bin/app.rb")),
            Some(Lang::Ruby)
        );
        assert_eq!(
            Lang::detect(None, Path::new("bin/Debug/net8.0/App.dll")),
            Some(Lang::CSharp)
        );
        for (hint, lang) in [
            ("rdbg", Lang::Ruby),
            ("ruby", Lang::Ruby),
            ("coreclr", Lang::CSharp),
            ("netcoredbg", Lang::CSharp),
            ("zig", Lang::Native),
            ("xdebug", Lang::Php),
            ("ghci", Lang::Haskell),
            ("java", Lang::Java),
            ("erlang", Lang::Erlang),
        ] {
            assert_eq!(
                Lang::detect(Some(hint), Path::new("x")),
                Some(lang),
                "{hint}"
            );
        }
        assert_eq!(
            Lang::detect(None, Path::new("public/index.php")),
            Some(Lang::Php)
        );
        assert_eq!(
            (Lang::Php.slug(), Lang::Php.label()),
            ("php", "PHP (Xdebug)")
        );
        assert_eq!(
            Lang::detect(None, Path::new("src/shop.erl")),
            Some(Lang::Erlang)
        );
        assert_eq!(
            Lang::detect(None, Path::new("app/Main.hs")),
            Some(Lang::Haskell)
        );
        assert_eq!(
            (Lang::Erlang.slug(), Lang::Haskell.slug()),
            ("erlang", "haskell")
        );
        // Each goes to its own adapter, by the slug the resolver matches.
        assert_eq!((Lang::Ruby.slug(), Lang::CSharp.slug()), ("ruby", "csharp"));
        assert_eq!(Lang::Ruby.label(), "Ruby (rdbg)");
    }

    /// The venv is the PROJECT's, whatever directory the launch configuration
    /// runs the program in: a `launch.json` whose `cwd` is a subdirectory is
    /// resolved against the root's `.venv` — it used to be looked for in the
    /// `cwd`, so the program was debugged under whatever `python3` came first
    /// on PATH, without the project's packages — and the program still runs
    /// in that subdirectory.
    #[test]
    #[cfg(unix)]
    fn a_launch_config_with_a_subdirectory_cwd_debugs_under_the_projects_venv() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = crate::app::tests::test_dir("dap-launch-cwd");
        // The base interpreter a venv links to, outside the project; it has
        // debugpy and runs a venv (the probe's answer, `1 1`).
        let base = scratch.join("base/bin/python3");
        std::fs::create_dir_all(base.parent().unwrap()).unwrap();
        std::fs::write(&base, "#!/bin/sh\ncase \"$1\" in -c) echo '1 1' ;; esac\n").unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Run once bare, so the probe's deadline does not time the OS's
        // first-run check of this new file.
        clew_core::testutil::settle_new_executable(&base);
        let root = scratch.join("project");
        let venv_python = root.join(".venv/bin/python");
        std::fs::create_dir_all(venv_python.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&base, &venv_python).unwrap();
        std::fs::create_dir_all(root.join("tools")).unwrap();
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        std::fs::write(
            root.join(".clew/launch.json"),
            r#"{"type": "python", "program": "tools/run.py", "cwd": "tools", "args": ["--fast"]}"#,
        )
        .unwrap();

        let cfg = crate::app::tasks::read_launch_config(&root).unwrap();
        assert_eq!(cfg.cwd, root.join("tools"));
        let lang = Lang::detect(cfg.type_hint.as_deref(), &cfg.program).unwrap();
        let resolved = resolve(
            lang,
            &cfg.program,
            &cfg.args,
            &root,
            &cfg.cwd,
            &Default::default(),
        )
        .unwrap();
        let Resolved::Ready(adapter) = resolved else {
            panic!("debugpy is in the project's venv, yet an install was asked for");
        };
        assert_eq!(
            adapter.command, venv_python,
            "not the project's interpreter"
        );
        assert_eq!(
            adapter.launch["cwd"],
            root.join("tools").to_string_lossy().as_ref()
        );
        assert_eq!(adapter.launch["args"][0], "--fast");
    }
}
