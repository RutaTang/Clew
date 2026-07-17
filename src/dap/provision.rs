//! Debug-adapter provisioning: where clew keeps downloadable adapters, and how
//! it installs the auto-provisionable ones. Most adapters are located from the
//! environment (see [`super::adapter`]); the ones that need downloading live
//! under clew's data dir alongside the LSP servers.

use std::path::{Path, PathBuf};

/// Root under clew's data dir where downloadable debug adapters live.
fn adapters_root() -> Option<PathBuf> {
    Some(crate::lsp::store::data_root()?.join("debug-adapters"))
}

/// The provisioned vscode-js-debug DAP server entrypoint, if installed.
pub fn js_debug_server() -> Option<PathBuf> {
    let p = adapters_root()?.join("js-debug").join("src").join("dapDebugServer.js");
    p.is_file().then_some(p)
}

/// Install debugpy into `python` (blocking; run off the UI thread). Used to
/// auto-provision Python debugging on first use.
pub fn install_debugpy(python: &Path) -> Result<(), String> {
    let out = std::process::Command::new(python)
        .args(["-m", "pip", "install", "--user", "debugpy"])
        .output()
        .map_err(|e| format!("pip: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr)
            .lines()
            .next_back()
            .unwrap_or("pip install debugpy failed")
            .to_string())
    }
}
