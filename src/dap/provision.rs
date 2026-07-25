//! Debug-adapter provisioning: where clew keeps downloadable adapters, and how
//! it installs the auto-provisionable ones. Most adapters are located from the
//! environment (see [`super::adapter`]); the ones that need downloading live
//! under clew's data dir alongside the LSP servers.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Pinned vscode-js-debug release — downloaded once, verified before use.
const JS_DEBUG_URL: &str = "https://github.com/microsoft/vscode-js-debug/releases/download/v1.117.0/js-debug-dap-v1.117.0.tar.gz";
const JS_DEBUG_SHA256: &str = "ad8d04ede9d4b75cc290fd5438a65047a06f786d04f604b6112485b36f090772";

/// Root under clew's data dir where downloadable debug adapters live.
fn adapters_root() -> Option<PathBuf> {
    Some(crate::lsp::store::data_root()?.join("debug-adapters"))
}

/// The provisioned vscode-js-debug DAP server entrypoint, if installed.
pub fn js_debug_server() -> Option<PathBuf> {
    let p = adapters_root()?
        .join("js-debug")
        .join("src")
        .join("dapDebugServer.js");
    p.is_file().then_some(p)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Download, verify (pinned SHA-256), and extract vscode-js-debug into the data
/// dir. Blocking — run off the UI thread. Never extracts unverified bytes.
pub fn install_js_debug() -> Result<PathBuf, String> {
    let root = adapters_root().ok_or("no data directory")?;
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let resp = ureq::get(JS_DEBUG_URL)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read failed: {e}"))?;
    let got = hex(&Sha256::digest(&bytes));
    if got != JS_DEBUG_SHA256 {
        return Err(format!("js-debug checksum mismatch (got {got})"));
    }
    // The tarball has a top-level `js-debug/`, so it lands at debug-adapters/js-debug/.
    let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    tar::Archive::new(gz)
        .unpack(&root)
        .map_err(|e| format!("extract failed: {e}"))?;
    js_debug_server().ok_or_else(|| "js-debug extracted but server not found".to_string())
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
