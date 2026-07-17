//! Per-language debug-adapter resolution: maps a program + language to the DAP
//! adapter binary to spawn (over stdio) and the `launch` request arguments that
//! adapter expects. This is the seam that makes clew's debugger multi-language —
//! the DAP client and UI are identical across languages; only the adapter and
//! the launch-argument shape differ.
//!
//! Adapters are located from the environment (PATH / known SDK paths); when one
//! is missing, the error explains how to install it. Auto-provisioning of the
//! downloadable ones lives in [`super::provision`].

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// A resolved adapter ready to spawn, plus the launch request for the program.
pub struct Adapter {
    /// The DAP adapter executable.
    pub command: PathBuf,
    /// Arguments to the adapter process itself (not the debuggee).
    pub args: Vec<String>,
    /// The `launch` request arguments (adapter-specific shape).
    pub launch: Value,
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
}

impl Lang {
    /// Pick the language from an explicit launch.json `type`, else the program's
    /// file extension (no extension ⇒ a compiled native binary).
    pub fn detect(type_hint: Option<&str>, program: &Path) -> Option<Lang> {
        if let Some(t) = type_hint {
            return match t.to_ascii_lowercase().as_str() {
                "rust" | "lldb" | "lldb-dap" | "cpp" | "c" | "codelldb" | "native" => {
                    Some(Lang::Native)
                }
                "python" | "debugpy" => Some(Lang::Python),
                "go" | "delve" | "dlv" => Some(Lang::Go),
                "dart" | "flutter" => Some(Lang::Dart),
                "node" | "js" | "ts" | "javascript" | "typescript" | "pwa-node" => Some(Lang::Node),
                _ => None,
            };
        }
        match program.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("py") => Some(Lang::Python),
            Some("dart") => Some(Lang::Dart),
            Some("go") => Some(Lang::Go),
            Some("js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx") => Some(Lang::Node),
            None => Some(Lang::Native),
            _ => Some(Lang::Native),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Lang::Native => "native (lldb)",
            Lang::Python => "Python (debugpy)",
            Lang::Go => "Go (delve)",
            Lang::Dart => "Dart",
            Lang::Node => "Node (js-debug)",
        }
    }
}

/// Find an executable on `PATH`.
fn which(exe: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|d| d.join(exe)).find(|p| p.is_file())
    })
}

/// Resolve the adapter + launch args for a program in `lang`.
pub fn resolve(lang: Lang, program: &Path, args: &[String], cwd: &Path) -> Result<Adapter, String> {
    match lang {
        Lang::Native => native(program, args, cwd),
        Lang::Python => python(program, args, cwd),
        Lang::Dart => dart(program, args, cwd),
        Lang::Go => go(program, args, cwd),
        Lang::Node => node(program, args, cwd),
    }
}

/// lldb-dap for native (Rust/C/C++) binaries. Ships with Xcode / LLVM.
fn native(program: &Path, args: &[String], cwd: &Path) -> Result<Adapter, String> {
    let command = which("lldb-dap")
        .or_else(|| xcrun("lldb-dap"))
        .ok_or("lldb-dap not found — install Xcode command-line tools or LLVM")?;
    Ok(Adapter {
        command,
        args: Vec::new(),
        launch: json!({
            "program": program.to_string_lossy(),
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "stopOnEntry": false,
        }),
    })
}

/// debugpy: `python -m debugpy.adapter` speaks DAP over stdio and debugs the
/// same interpreter it runs under.
fn python(program: &Path, args: &[String], cwd: &Path) -> Result<Adapter, String> {
    let py = which("python3").or_else(|| which("python")).ok_or("python not found")?;
    let importable = |py: &Path| {
        std::process::Command::new(py)
            .args(["-c", "import debugpy"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    // Auto-provision debugpy into that interpreter on first use.
    if !importable(&py) {
        super::provision::install_debugpy(&py)?;
        if !importable(&py) {
            return Err(format!("installed debugpy but {} still can't import it", py.display()));
        }
    }
    Ok(Adapter {
        command: py.clone(),
        args: vec!["-m".into(), "debugpy.adapter".into()],
        launch: json!({
            "request": "launch",
            "program": program.to_string_lossy(),
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "console": "internalConsole",
            "python": [py.to_string_lossy()],
            "stopOnEntry": false,
            "justMyCode": false,
        }),
    })
}

/// Dart's SDK ships a DAP adapter: `dart debug_adapter` over stdio.
fn dart(program: &Path, args: &[String], cwd: &Path) -> Result<Adapter, String> {
    let command = which("dart").ok_or("dart not found — install the Dart/Flutter SDK")?;
    Ok(Adapter {
        command,
        args: vec!["debug_adapter".into()],
        launch: json!({
            "request": "launch",
            "program": program.to_string_lossy(),
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "toolArgs": [],
        }),
    })
}

/// Delve for Go: `dlv dap` over stdio (mode=debug compiles+runs the package).
fn go(program: &Path, args: &[String], cwd: &Path) -> Result<Adapter, String> {
    let command = which("dlv")
        .ok_or("dlv not found — run: go install github.com/go-delve/delve/cmd/dlv@latest")?;
    Ok(Adapter {
        command,
        args: vec!["dap".into()],
        launch: json!({
            "request": "launch",
            "mode": "debug",
            "program": program.to_string_lossy(),
            "args": args,
            "cwd": cwd.to_string_lossy(),
        }),
    })
}

/// vscode-js-debug for JS/TS: node runs the provisioned dapDebugServer over
/// stdio (passing `0` makes it use stdio rather than a TCP port).
fn node(program: &Path, args: &[String], cwd: &Path) -> Result<Adapter, String> {
    let node = which("node").ok_or("node not found — install Node.js")?;
    let server = super::provision::js_debug_server()
        .ok_or("js-debug not installed — clew can download it (Debug a .js/.ts file to prompt)")?;
    Ok(Adapter {
        command: node,
        args: vec![server.to_string_lossy().into_owned()],
        launch: json!({
            "type": "pwa-node",
            "request": "launch",
            "program": program.to_string_lossy(),
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "console": "internalConsole",
        }),
    })
}

/// Locate a tool in the active Xcode toolchain via `xcrun -f`.
fn xcrun(tool: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("xcrun").args(["-f", tool]).output().ok()?;
    if out.status.success() {
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !p.is_empty() && Path::new(&p).is_file() {
            return Some(PathBuf::from(p));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_language_by_type_then_extension() {
        assert_eq!(Lang::detect(Some("python"), Path::new("x")), Some(Lang::Python));
        assert_eq!(Lang::detect(Some("go"), Path::new("x")), Some(Lang::Go));
        assert_eq!(Lang::detect(None, Path::new("a/main.py")), Some(Lang::Python));
        assert_eq!(Lang::detect(None, Path::new("a/main.dart")), Some(Lang::Dart));
        assert_eq!(Lang::detect(None, Path::new("a/app.ts")), Some(Lang::Node));
        // No extension ⇒ a compiled native binary.
        assert_eq!(Lang::detect(None, Path::new("target/debug/app")), Some(Lang::Native));
    }
}
