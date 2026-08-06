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
use super::registry::{self, Archive, Download, Install, Platform, Provision};

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

/// Whether `s` is safe to use as one directory name under the store. The name
/// and version can come from the project's `lsp.toml`, so a path separator,
/// `..`, or an absolute path would let a repository steer the store's install
/// path — and installs delete their destination before writing it.
fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && !s.contains(['/', '\\', '\0'])
        // Reject anything that isn't exactly one normal path component
        // (catches Windows drive prefixes and other oddities too).
        && std::path::Path::new(s)
            .components()
            .eq(std::iter::once(std::path::Component::Normal(s.as_ref())))
}

fn server_dir(name: &str, version: &str) -> Option<PathBuf> {
    if !safe_component(name) || !safe_component(version) {
        return None;
    }
    Some(data_root()?.join("servers").join(name).join(version))
}

/// Refuse to touch a path that is not inside the server store. Used before any
/// destructive step (installs wipe their destination first), so a store path
/// built from untrusted input can never escape — even via a symlinked parent.
fn inside_store(path: &Path) -> bool {
    let Some(store) = data_root().map(|r| r.join("servers")) else {
        return false;
    };
    // Canonicalize as far as the path exists (the leaf usually doesn't yet),
    // so a symlinked ancestor can't redirect the write out of the store.
    let mut existing = path;
    while !existing.exists() {
        match existing.parent() {
            Some(p) => existing = p,
            None => return false,
        }
    }
    let (Ok(real), Ok(real_store)) = (existing.canonicalize(), store.canonicalize()) else {
        // No store directory yet: accept only a path that is lexically under it.
        return path.starts_with(&store);
    };
    real.starts_with(&real_store) && path.starts_with(&store)
}

/// A server present in the global store.
#[derive(Debug, Clone)]
pub struct InstalledServer {
    pub name: String,
    pub version: String,
    pub bytes: u64,
}

/// List every installed server in the global store, with its disk usage.
pub fn installed_servers() -> Vec<InstalledServer> {
    let Some(servers) = data_root().map(|r| r.join("servers")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let Ok(names) = std::fs::read_dir(&servers) else {
        return out;
    };
    for name_entry in names.flatten() {
        let name = name_entry.file_name().to_string_lossy().into_owned();
        let Ok(versions) = std::fs::read_dir(name_entry.path()) else {
            continue;
        };
        for ver_entry in versions.flatten() {
            if !ver_entry.path().is_dir() {
                continue;
            }
            out.push(InstalledServer {
                name: name.clone(),
                version: ver_entry.file_name().to_string_lossy().into_owned(),
                bytes: dir_size(&ver_entry.path()),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
    out
}

/// Remove an installed server from the store. Returns whether anything existed.
pub fn remove(name: &str, version: &str) -> Result<bool, String> {
    let Some(dir) = server_dir(name, version) else {
        return Ok(false);
    };
    if !dir.exists() {
        return Ok(false);
    }
    if !inside_store(&dir) {
        return Err("refusing to remove a path outside the server store".into());
    }
    std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    // Prune the now-empty parent name directory.
    if let Some(parent) = dir.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(true)
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            _ => e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .sum()
}

/// Outcome of locating the binary for an effective server config.
#[derive(Debug, Clone)]
pub enum Located {
    /// Ready to launch (custom command or already installed).
    Ready(PathBuf),
    /// Not installed yet; needs a consent-gated download of a verified binary.
    NeedsDownload {
        download: Download,
        dest_dir: PathBuf,
    },
    /// Not installed yet; needs a consent-gated toolchain install.
    NeedsInstall { install: Install, dest_dir: PathBuf },
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
    let Some(provision) = spec.provision(platform) else {
        return Located::Unsupported(format!(
            "{} is not available for this platform",
            server.server_name
        ));
    };
    // A toolchain-bundled server (e.g. `dart language-server`) is run from the
    // toolchain binary on PATH — nothing to download or install.
    if let Provision::Toolchain { binary } = &provision {
        return match find_on_path(binary) {
            Some(p) => Located::Ready(p),
            None => Located::Unsupported(format!(
                "'{binary}' not found on PATH — install the toolchain (e.g. the Dart/Flutter SDK)"
            )),
        };
    }
    let Some(dest_dir) = server_dir(&server.server_name, &server.version) else {
        return Located::Unsupported("no data directory".into());
    };
    let binary = match &provision {
        Provision::Download(d) => dest_dir.join(d.binary),
        Provision::Install(i) => dest_dir.join(i.binary),
        Provision::Toolchain { .. } => unreachable!("handled above"),
    };
    if binary.is_file() {
        return Located::Ready(binary);
    }
    match provision {
        Provision::Download(download) => Located::NeedsDownload { download, dest_dir },
        Provision::Install(install) => Located::NeedsInstall { install, dest_dir },
        Provision::Toolchain { .. } => unreachable!("handled above"),
    }
}

/// The first directory on `PATH` containing an executable named `binary`.
fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|p| p.is_file())
}

/// Run a toolchain installer, placing the server in `dest_dir`. Blocking.
pub fn toolchain_install(
    install: &Install,
    version: &str,
    dest_dir: &Path,
) -> Result<PathBuf, String> {
    use registry::Installer;

    // Same guard as `install_bytes`: this wipes `dest_dir` first, and the path
    // is derived from possibly project-supplied name/version.
    if !inside_store(dest_dir) {
        return Err("refusing to install outside the server store".into());
    }
    // Start from a clean directory so a re-install can't inherit stale files.
    let _ = std::fs::remove_dir_all(dest_dir);
    std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;

    let mut cmd = std::process::Command::new(match &install.kind {
        Installer::Go { .. } => "go",
        Installer::Npm { .. } => "npm",
        Installer::Cargo { .. } => "cargo",
    });
    match &install.kind {
        Installer::Go { module } => {
            cmd.args(["install", &format!("{module}@{version}")])
                .env("GOBIN", dest_dir);
        }
        Installer::Npm { packages } => {
            cmd.arg("install").arg("--prefix").arg(dest_dir);
            for pkg in *packages {
                // `latest` means "no pin"; npm resolves the newest.
                if version == "latest" {
                    cmd.arg(pkg);
                } else {
                    cmd.arg(format!("{pkg}@{version}"));
                }
            }
        }
        Installer::Cargo {
            crate_name,
            features,
        } => {
            // `--root <dir>` installs the binary at <dir>/bin/<name>.
            cmd.arg("install")
                .arg(crate_name)
                .arg("--root")
                .arg(dest_dir);
            if version != "latest" {
                cmd.arg("--version").arg(version);
            }
            if !features.is_empty() {
                cmd.arg("--features").arg(features.join(","));
            }
        }
    }

    let output = cmd.output().map_err(|e| {
        format!(
            "'{}' is required to install {} but was not found ({e}); \
             install {0}, or set a custom `command` in .clew/lsp.toml",
            install.tool, install.tool
        )
    })?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_dir_all(dest_dir);
        return Err(format!("{}: {}", install.describe, err.trim()));
    }

    let binary = dest_dir.join(install.binary);
    if !binary.is_file() {
        return Err(format!(
            "install completed but '{}' is missing",
            install.binary
        ));
    }
    // The install may already be executable; ensure it for good measure.
    let _ = make_executable(&binary);
    Ok(binary)
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
///
/// gzip artifacts are a single executable written as `dest_dir/<binary>`; zip
/// artifacts are extracted whole (some servers, e.g. clangd, need sibling
/// resource files), with the executable at the relative path `<binary>`.
fn install_bytes(bytes: &[u8], download: &Download, dest_dir: &Path) -> Result<PathBuf, String> {
    // The destination is derived from a name/version that can come from the
    // project's lsp.toml, and installing wipes it first — never touch a path
    // outside the store.
    if !inside_store(dest_dir) {
        return Err("refusing to install outside the server store".into());
    }
    // Verify before we ever unpack or execute anything.
    if download.sha256.is_empty() {
        return Err("no checksum for this version; refusing to install".into());
    }
    let actual = hex_sha256(bytes);
    if !actual.eq_ignore_ascii_case(download.sha256) {
        return Err(format!(
            "checksum mismatch: expected {}, got {actual}",
            download.sha256
        ));
    }

    // Stage into a temp dir, then rename into place atomically.
    let parent = dest_dir
        .parent()
        .ok_or_else(|| "invalid destination".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = parent.join(format!(
        ".tmp-{}-{}",
        dest_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("srv"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;

    let result = (|| {
        match download.archive {
            Archive::Gzip => {
                let mut exe = Vec::new();
                flate2::read::GzDecoder::new(bytes)
                    .read_to_end(&mut exe)
                    .map_err(|e| format!("gunzip failed: {e}"))?;
                std::fs::write(tmp.join(download.binary), &exe).map_err(|e| e.to_string())?;
            }
            Archive::Zip => extract_zip(bytes, &tmp)?,
            Archive::TarXz => extract_tar_xz(bytes, &tmp)?,
        }
        let exe_path = tmp.join(download.binary);
        if !exe_path.is_file() {
            return Err(format!("'{}' not found in archive", download.binary));
        }
        make_executable(&exe_path)?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }

    // Replace any partial/old install, then move ours in atomically.
    let _ = std::fs::remove_dir_all(dest_dir);
    std::fs::rename(&tmp, dest_dir).map_err(|e| {
        let _ = std::fs::remove_dir_all(&tmp);
        format!("install failed: {e}")
    })?;
    Ok(dest_dir.join(download.binary))
}

/// Extract an xz-compressed tar into `dest`, preserving its tree.
fn extract_tar_xz(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let mut tar_bytes = Vec::new();
    lzma_rs::xz_decompress(&mut std::io::Cursor::new(bytes), &mut tar_bytes)
        .map_err(|e| format!("xz decode failed: {e}"))?;
    tar::Archive::new(std::io::Cursor::new(tar_bytes))
        .unpack(dest)
        .map_err(|e| format!("tar extract failed: {e}"))
}

/// Extract every file of a zip archive into `dest`, preserving its tree.
fn extract_zip(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("bad zip: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        // Guard against path traversal in archive entry names.
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut file = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut file).map_err(|e| e.to_string())?;
    }
    Ok(())
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
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())
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

    /// Point the store at a fresh temp data dir and run `f` with it. Installs
    /// are confined to the store, so tests must install where a real one does.
    /// Holds the env lock for the duration (CLEW_DATA_DIR is process-global).
    fn with_store<T>(name: &str, f: impl FnOnce(&Path) -> T) -> T {
        let _env = crate::env_lock();
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("servers")).unwrap();
        // SAFETY: env mutation serialized by env_lock.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &dir) };
        let out = f(&dir);
        unsafe { std::env::remove_var("CLEW_DATA_DIR") };
        out
    }

    #[test]
    fn install_verifies_checksum_and_unpacks() {
        with_store("clew-store-test-ok", |dir| {
            let payload = b"#!/bin/sh\necho fake-server\n";
            let gz = gzip(payload);
            let download = Download {
                url: "https://example/x.gz".into(),
                sha256: Box::leak(hex_sha256(&gz).into_boxed_str()),
                archive: Archive::Gzip,
                binary: "rust-analyzer",
            };
            let dest = dir.join("servers/rust-analyzer/2026-07-13");
            let installed = install_bytes(&gz, &download, &dest).unwrap();
            assert_eq!(installed, dest.join("rust-analyzer"));
            assert_eq!(std::fs::read(&installed).unwrap(), payload);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
                assert_eq!(mode & 0o111, 0o111, "must be executable");
            }
        });
    }

    #[test]
    fn install_refuses_a_destination_outside_the_store() {
        with_store("clew-store-outside", |dir| {
            let gz = gzip(b"payload");
            let download = Download {
                url: "https://example/x.gz".into(),
                sha256: Box::leak(hex_sha256(&gz).into_boxed_str()),
                archive: Archive::Gzip,
                binary: "rust-analyzer",
            };
            // Installing wipes its destination first, so a path steered out of
            // the store (e.g. by a `version` from the project's lsp.toml) must
            // be refused before anything is deleted.
            let victim = dir.join("precious");
            std::fs::create_dir_all(&victim).unwrap();
            std::fs::write(victim.join("keep.txt"), b"keep").unwrap();
            let err = install_bytes(&gz, &download, &victim).unwrap_err();
            assert!(err.contains("outside the server store"), "{err}");
            assert!(victim.join("keep.txt").is_file(), "must not be wiped");
        });
    }

    #[test]
    fn install_zip_extracts_tree_and_finds_nested_binary() {
        use std::io::Write;
        // Build a zip with a nested binary and a sibling resource file.
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            zw.start_file("clangd_1.0/bin/clangd", opts).unwrap();
            zw.write_all(b"#!/bin/sh\necho clangd\n").unwrap();
            zw.start_file("clangd_1.0/lib/clang/headers.h", opts)
                .unwrap();
            zw.write_all(b"// header").unwrap();
            zw.finish().unwrap();
        }
        let download = Download {
            url: "https://example/c.zip".into(),
            sha256: Box::leak(hex_sha256(&buf).into_boxed_str()),
            archive: Archive::Zip,
            binary: "clangd_1.0/bin/clangd",
        };
        with_store("clew-store-zip", |dir| {
            let dest = dir.join("servers/clangd/1.0");
            let installed = install_bytes(&buf, &download, &dest).unwrap();
            assert_eq!(installed, dest.join("clangd_1.0/bin/clangd"));
            assert!(installed.is_file());
            // The sibling resource tree came along.
            assert!(dest.join("clangd_1.0/lib/clang/headers.h").is_file());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
                assert_eq!(mode & 0o111, 0o111);
            }
        });
    }

    #[test]
    fn install_tar_xz_extracts_binary() {
        // Build a tar containing a `zls` binary, xz-compress it.
        let mut tar = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar);
            let payload = b"#!/bin/sh\necho zls\n";
            let mut header = tar::Header::new_gnu();
            header.set_path("zls").unwrap();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append(&header, &payload[..]).unwrap();
            builder.finish().unwrap();
        }
        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut std::io::Cursor::new(&tar), &mut xz).unwrap();

        let download = Download {
            url: "https://example/zls.tar.xz".into(),
            sha256: Box::leak(hex_sha256(&xz).into_boxed_str()),
            archive: Archive::TarXz,
            binary: "zls",
        };
        with_store("clew-store-tarxz", |dir| {
            let dest = dir.join("servers/zls/0.16.0");
            let installed = install_bytes(&xz, &download, &dest).unwrap();
            assert_eq!(installed, dest.join("zls"));
            assert!(installed.is_file());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
                assert_eq!(mode & 0o111, 0o111);
            }
        });
    }

    #[test]
    fn install_rejects_bad_checksum() {
        with_store("clew-store-test-bad", |dir| {
            let gz = gzip(b"payload");
            let download = Download {
                url: "https://example/x.gz".into(),
                sha256: "0000000000000000000000000000000000000000000000000000000000000000",
                archive: Archive::Gzip,
                binary: "rust-analyzer",
            };
            let dest = dir.join("servers/s/v");
            let err = install_bytes(&gz, &download, &dest).unwrap_err();
            assert!(err.contains("checksum mismatch"), "{err}");
            assert!(!dest.exists(), "must not install on mismatch");
        });
    }

    #[test]
    fn empty_checksum_refused() {
        with_store("clew-store-nochecksum", |dir| {
            let gz = gzip(b"x");
            let download = Download {
                url: "https://example/x.gz".into(),
                sha256: "",
                archive: Archive::Gzip,
                binary: "rust-analyzer",
            };
            let err = install_bytes(&gz, &download, &dir.join("servers/s/v")).unwrap_err();
            assert!(err.contains("no checksum"), "{err}");
        });
    }

    #[test]
    fn store_paths_reject_traversal_from_project_config() {
        let _env = crate::env_lock();
        let dir = std::env::temp_dir().join("clew-store-traversal");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: env mutation serialized by env_lock.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &dir) };

        // A version (or name) from the project's lsp.toml must not steer the
        // install path: installs wipe their destination before writing it.
        assert!(server_dir("rust-analyzer", "../../../../etc").is_none());
        assert!(server_dir("rust-analyzer", "/tmp/anywhere").is_none());
        assert!(server_dir("../evil", "1").is_none());
        assert!(server_dir("rust-analyzer", "").is_none());
        assert!(server_dir("rust-analyzer", "..").is_none());
        assert!(server_dir("a/b", "1").is_none());
        // A normal name/version still resolves under the store.
        let ok = server_dir("rust-analyzer", "2026-07-13").expect("normal path");
        assert!(ok.starts_with(dir.join("servers")));

        // …and the containment guard rejects anything outside the store even
        // if a path is constructed some other way.
        assert!(!inside_store(&dir.join("elsewhere")));
        assert!(!inside_store(std::path::Path::new("/tmp")));
        assert!(inside_store(&ok));

        // `remove` refuses such a path rather than deleting it.
        let victim = dir.join("precious");
        std::fs::create_dir_all(&victim).unwrap();
        assert!(remove("rust-analyzer", "../precious").is_ok_and(|removed| !removed));
        assert!(
            victim.exists(),
            "traversal must not delete outside the store"
        );

        unsafe { std::env::remove_var("CLEW_DATA_DIR") };
    }

    #[test]
    fn data_root_and_server_dir_honor_override() {
        let _env = crate::env_lock();
        // SAFETY: env mutation serialized by env_lock.
        unsafe { std::env::set_var("CLEW_DATA_DIR", "/tmp/clew-xyz") };
        assert_eq!(data_root(), Some(PathBuf::from("/tmp/clew-xyz")));
        assert_eq!(
            server_dir("rust-analyzer", "2026-07-13"),
            Some(PathBuf::from(
                "/tmp/clew-xyz/servers/rust-analyzer/2026-07-13"
            ))
        );
        unsafe { std::env::remove_var("CLEW_DATA_DIR") };
    }
}

#[cfg(test)]
mod toolchain_tests {
    use super::*;
    use crate::lsp::registry::{self, Provision};

    #[test]
    #[ignore] // runs a real `npm install`; run explicitly
    fn npm_install_typescript_language_server() {
        let spec = registry::by_name("typescript-language-server").unwrap();
        let Provision::Install(install) = spec.provision(Platform::current().unwrap()).unwrap()
        else {
            panic!("expected a toolchain install");
        };
        let dir = std::env::temp_dir().join("clew-npm-ts-test");
        let _ = std::fs::remove_dir_all(&dir);
        let bin = toolchain_install(&install, "latest", &dir).expect("install");
        assert!(bin.is_file(), "expected {bin:?}");
        assert!(bin.ends_with("node_modules/.bin/typescript-language-server"));
        let out = std::process::Command::new(&bin)
            .arg("--version")
            .output()
            .unwrap();
        assert!(out.status.success(), "server --version failed");
        eprintln!(
            "installed: {bin:?} -> {}",
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
}

#[cfg(test)]
mod npm_config_tests {
    use super::*;
    use crate::lsp::registry::{self, Provision};

    fn install_and_check(server: &str, expect_bin_ends: &str) {
        let spec = registry::by_name(server).unwrap();
        let Provision::Install(install) = spec.provision(Platform::current().unwrap()).unwrap()
        else {
            panic!("{server} should be a toolchain install");
        };
        let dir = std::env::temp_dir().join(format!("clew-npm-cfg-{server}"));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = toolchain_install(&install, "latest", &dir).expect("install");
        assert!(bin.is_file(), "expected bin at {bin:?}");
        assert!(bin.ends_with(expect_bin_ends), "{bin:?}");
        eprintln!("{server}: installed {bin:?}");
    }

    #[test]
    #[ignore] // runs real `npm install`; run explicitly
    fn config_language_servers_install() {
        install_and_check(
            "vscode-json-language-server",
            "node_modules/.bin/vscode-json-language-server",
        );
        install_and_check("taplo", "node_modules/.bin/taplo");
    }
}

#[cfg(test)]
mod cargo_toml_test {
    use super::*;
    use crate::lsp::registry::{self, Provision};

    #[test]
    #[ignore] // runs a real `cargo install` (compiles taplo); run explicitly
    fn cargo_install_taplo_has_lsp() {
        let spec = registry::by_name("taplo").unwrap();
        let Provision::Install(install) = spec.provision(Platform::current().unwrap()).unwrap()
        else {
            panic!("taplo should be a toolchain install");
        };
        let dir = std::env::temp_dir().join("clew-cargo-taplo");
        let bin = toolchain_install(&install, "latest", &dir).expect("cargo install taplo");
        assert!(bin.ends_with("bin/taplo"), "{bin:?}");

        // The native build must include the LSP subcommand (npm build didn't).
        let help = std::process::Command::new(&bin)
            .args(["lsp", "--help"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&help.stdout);
        assert!(text.contains("stdio"), "taplo lsp missing stdio: {text}");
        eprintln!("taplo with LSP installed at {bin:?}");
    }
}
