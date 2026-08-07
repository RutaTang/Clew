//! Server-side debug-adapter resolution, for the stdio-transport adapters a
//! remote host can proxy over the protocol.
//!
//! The client's own resolver (`src/dap/adapter.rs`) covers every transport
//! and may auto-provision; THIS one runs on the clew-server host — where the
//! debuggee actually lives — and is deliberately narrower:
//!   - stdio adapters only (lldb-dap, debugpy, `dart debug_adapter`): their
//!     frames proxy over `ProcessOutput`/`ProcessInput`. TCP adapters
//!     (delve, vscode-js-debug) listen on the REMOTE's loopback, which the
//!     client cannot reach — refused with a clear message.
//!   - no auto-provisioning: installing debugpy on a remote host without
//!     the user's explicit consent would violate the same rule LspInstall
//!     exists for. The error says what to install instead.

use std::path::{Path, PathBuf};

use serde_json::json;

/// A resolved stdio adapter: what to spawn, and the `launch` request body
/// (serialized — the client passes it to the adapter verbatim, with paths
/// that are native to THIS host).
#[derive(Debug)]
pub struct StdioAdapter {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub launch: String,
}

/// Resolve the stdio adapter for `lang` (a slug from the client's `Lang`)
/// against this host's environment. Blocking (runs `xcrun` / `python -c`).
pub fn resolve_stdio(
    lang: &str,
    program: &str,
    args: &[String],
    cwd: &Path,
) -> Result<StdioAdapter, String> {
    match lang {
        "native" => native(program, args, cwd),
        "python" => python(program, args, cwd),
        "dart" => dart(program, args, cwd),
        "go" | "node" => Err(format!(
            "the {lang} debug adapter speaks DAP over a TCP port on this host, \
             which a remote client cannot reach — remote debugging currently \
             supports native (lldb), Python, and Dart"
        )),
        other => Err(format!("no debug adapter for {other}")),
    }
}

/// Find an executable on `PATH`, canonicalized to the real absolute file.
/// Relative `PATH` entries are skipped (the adapter later runs with the
/// project as cwd, so a relative hit could name a different file at spawn
/// time — potentially one the debugged repo itself provides).
fn which(exe: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .filter(|d| d.is_absolute())
            .map(|d| d.join(exe))
            .find_map(|p| std::fs::canonicalize(&p).ok().filter(|c| c.is_file()))
    })
}

/// Locate a tool in the active Xcode toolchain via `xcrun -f` (macOS hosts).
fn xcrun(tool: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("xcrun")
        .args(["-f", tool])
        .output()
        .ok()?;
    if out.status.success() {
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !p.is_empty() && Path::new(&p).is_file() {
            return Some(PathBuf::from(p));
        }
    }
    None
}

fn native(program: &str, args: &[String], cwd: &Path) -> Result<StdioAdapter, String> {
    let command = which("lldb-dap")
        .or_else(|| xcrun("lldb-dap"))
        .ok_or("lldb-dap not found on the remote — install LLVM (or Xcode CLT) there")?;
    Ok(StdioAdapter {
        command,
        args: Vec::new(),
        launch: json!({
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "stopOnEntry": false,
        })
        .to_string(),
    })
}

fn python(program: &str, args: &[String], cwd: &Path) -> Result<StdioAdapter, String> {
    let py = which("python3")
        .or_else(|| which("python"))
        .ok_or("python not found on the remote")?;
    let importable = std::process::Command::new(&py)
        .args(["-c", "import debugpy"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    // Never auto-install on a remote host: that is running an installer
    // without the user's consent. Say what to do instead.
    if !importable {
        return Err(format!(
            "debugpy is not importable by {} on the remote — install it there \
             (pip install debugpy)",
            py.display()
        ));
    }
    Ok(StdioAdapter {
        command: py.clone(),
        args: vec!["-m".into(), "debugpy.adapter".into()],
        launch: json!({
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "console": "internalConsole",
            "python": [py.to_string_lossy()],
            "stopOnEntry": false,
            "justMyCode": false,
        })
        .to_string(),
    })
}

fn dart(program: &str, args: &[String], cwd: &Path) -> Result<StdioAdapter, String> {
    let command =
        which("dart").ok_or("dart not found on the remote — install the Dart/Flutter SDK there")?;
    Ok(StdioAdapter {
        command,
        args: vec!["debug_adapter".into()],
        launch: json!({
            "request": "launch",
            "program": program,
            "args": args,
            "cwd": cwd.to_string_lossy(),
            "toolArgs": [],
        })
        .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
