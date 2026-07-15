//! The global server store and consent-gated provisioning.
//!
//! Server binaries are shared across projects, keyed by `(name, version)`:
//! `<data-dir>/clew/servers/<name>/<version>/<binary>`. They are downloaded
//! once, verified against a pinned SHA-256 before ever being executed, and
//! installed atomically (unpack into a temp dir, then rename into place).

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::config::EffectiveServer;
use super::registry::{self, Archive, Download, Platform};

/// Root of clew's global data directory (`CLEW_DATA_DIR` overrides it).
pub fn data_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLEW_DATA_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    Some(match std::env::consts::OS {
        "macos" => home?.join("Library/Application Support/clew"),
        "windows" => PathBuf::from(std::env::var_os("APPDATA")?).join("clew"),
        _ => match std::env::var_os("XDG_DATA_HOME") {
            Some(x) => PathBuf::from(x).join("clew"),
            None => home?.join(".local/share/clew"),
        },
    })
}

fn server_dir(name: &str, version: &str) -> Option<PathBuf> {
    Some(data_root()?.join("servers").join(name).join(version))
}

/// Outcome of locating the binary for an effective server config.
#[derive(Debug, Clone)]
pub enum Located {
    /// Ready to launch (custom command or already installed).
    Ready(PathBuf),
    /// Not installed yet; needs a consent-gated download.
    NeedsDownload { download: Download, dest_dir: PathBuf },
    /// No server available for this platform/config.
    Unsupported(String),
}

/// Decide how to obtain the binary for `server` without touching the network.
pub fn locate(server: &EffectiveServer) -> Located {
    // Escape hatch: a custom command is used directly, bypassing the store.
    if let Some(cmd) = &server.command {
        return Located::Ready(cmd.clone());
    }
    let Some(platform) = Platform::current() else {
        return Located::Unsupported("unsupported platform".into());
    };
    let Some(spec) = registry::by_name(&server.server_name) else {
        return Located::Unsupported(format!("no managed server '{}'", server.server_name));
    };
    let Some(download) = spec.download(platform) else {
        return Located::Unsupported(format!(
            "{} is not published for this platform",
            server.server_name
        ));
    };
    let Some(dest_dir) = server_dir(&server.server_name, &server.version) else {
        return Located::Unsupported("no data directory".into());
    };
    let binary = dest_dir.join(download.binary);
    if binary.is_file() {
        Located::Ready(binary)
    } else {
        Located::NeedsDownload { download, dest_dir }
    }
}

/// Fetch, verify, and install a server. Blocking; run off the UI thread.
/// Returns the path to the installed executable.
pub fn download_and_install(download: &Download, dest_dir: &Path) -> Result<PathBuf, String> {
    let bytes = fetch(&download.url)?;
    install_bytes(&bytes, download, dest_dir)
}

/// Download the raw bytes of a URL over HTTPS (redirects followed).
fn fetch(url: &str) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://") {
        return Err("refusing non-HTTPS download URL".into());
    }
    let resp = ureq::get(url)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| format!("download read failed: {e}"))?;
    Ok(buf)
}

/// Verify the checksum, unpack, and atomically install `bytes` into `dest_dir`.
fn install_bytes(bytes: &[u8], download: &Download, dest_dir: &Path) -> Result<PathBuf, String> {
    // Verify before we ever unpack or execute anything.
    if !download.sha256.is_empty() {
        let actual = hex_sha256(bytes);
        if !actual.eq_ignore_ascii_case(download.sha256) {
            return Err(format!(
                "checksum mismatch: expected {}, got {actual}",
                download.sha256
            ));
        }
    } else {
        return Err("no checksum for this version; refusing to install".into());
    }

    let exe = extract(bytes, download.archive, download.binary)?;

    // Atomic install: write into a temp dir, then rename into place.
    let parent = dest_dir
        .parent()
        .ok_or_else(|| "invalid destination".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = parent.join(format!(
        ".tmp-{}-{}",
        dest_dir.file_name().and_then(|n| n.to_str()).unwrap_or("srv"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let tmp_bin = tmp.join(download.binary);
    std::fs::write(&tmp_bin, &exe).map_err(|e| e.to_string())?;
    make_executable(&tmp_bin)?;

    // Replace any partial/old install, then move ours in atomically.
    let _ = std::fs::remove_dir_all(dest_dir);
    std::fs::rename(&tmp, dest_dir).map_err(|e| {
        let _ = std::fs::remove_dir_all(&tmp);
        format!("install failed: {e}")
    })?;
    Ok(dest_dir.join(download.binary))
}

/// Unpack the executable bytes from a downloaded artifact.
fn extract(bytes: &[u8], archive: Archive, _binary: &str) -> Result<Vec<u8>, String> {
    match archive {
        Archive::Gzip => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(bytes)
                .read_to_end(&mut out)
                .map_err(|e| format!("gunzip failed: {e}"))?;
            Ok(out)
        }
        Archive::Zip => Err("zip extraction is not yet supported".into()),
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn install_verifies_checksum_and_unpacks() {
        let dir = std::env::temp_dir().join("clew-store-test-ok");
        let _ = std::fs::remove_dir_all(&dir);
        let payload = b"#!/bin/sh\necho fake-server\n";
        let gz = gzip(payload);
        let download = Download {
            url: "https://example/x.gz".into(),
            sha256: Box::leak(hex_sha256(&gz).into_boxed_str()),
            archive: Archive::Gzip,
            binary: "rust-analyzer",
        };
        let dest = dir.join("rust-analyzer/2026-07-13");
        let installed = install_bytes(&gz, &download, &dest).unwrap();
        assert_eq!(installed, dest.join("rust-analyzer"));
        assert_eq!(std::fs::read(&installed).unwrap(), payload);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "must be executable");
        }
    }

    #[test]
    fn install_rejects_bad_checksum() {
        let dir = std::env::temp_dir().join("clew-store-test-bad");
        let _ = std::fs::remove_dir_all(&dir);
        let gz = gzip(b"payload");
        let download = Download {
            url: "https://example/x.gz".into(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            archive: Archive::Gzip,
            binary: "rust-analyzer",
        };
        let err = install_bytes(&gz, &download, &dir.join("s/v")).unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(!dir.join("s/v").exists(), "must not install on mismatch");
    }

    #[test]
    fn empty_checksum_refused() {
        let gz = gzip(b"x");
        let download = Download {
            url: "https://example/x.gz".into(),
            sha256: "",
            archive: Archive::Gzip,
            binary: "rust-analyzer",
        };
        let err = install_bytes(&gz, &download, Path::new("/tmp/none")).unwrap_err();
        assert!(err.contains("no checksum"), "{err}");
    }

    #[test]
    fn data_root_and_server_dir_honor_override() {
        // SAFETY: test-only single-threaded env mutation.
        unsafe { std::env::set_var("CLEW_DATA_DIR", "/tmp/clew-xyz") };
        assert_eq!(data_root(), Some(PathBuf::from("/tmp/clew-xyz")));
        assert_eq!(
            server_dir("rust-analyzer", "2026-07-13"),
            Some(PathBuf::from("/tmp/clew-xyz/servers/rust-analyzer/2026-07-13"))
        );
        unsafe { std::env::remove_var("CLEW_DATA_DIR") };
    }
}
