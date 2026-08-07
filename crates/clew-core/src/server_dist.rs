//! Obtaining the clew-server binary for a remote's platform — automatically.
//!
//! A user never installs clew-server by hand. When connecting to a remote, the
//! client downloads the prebuilt binary for that remote's platform from the
//! release host (caching it locally), then deploys it over SSH. This mirrors how
//! VS Code ships its remote server. The release host is overridable via
//! `CLEW_SERVER_DIST_URL` (used by tests / self-hosting).
//!
//! The asset name embeds the protocol version (`clew-server-<slug>-p<proto>`),
//! so a release can only ever satisfy a client that speaks its protocol: a
//! client whose protocol moved past the published release gets a clean "no
//! such asset" error instead of deploying a binary whose every frame fails to
//! deserialize. Each asset ships with a `.sha256` sidecar; the download is
//! verified against it before it is cached or deployed, and the cache is
//! re-verified on every reuse. (The sidecar comes from the same host over
//! TLS, so this guards transfer corruption and local cache tampering — it is
//! not a signature.)

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::lsp::store::data_root;

/// Hard cap on a downloaded server binary (the static musl build is ~20 MB).
/// A response larger than this is broken or hostile; reading it to the end
/// would balloon memory before any other check can reject it.
const MAX_SERVER_BYTES: u64 = 100 * 1024 * 1024;

/// Where prebuilt clew-server binaries are published, one per platform slug
/// and protocol: `<base>/v<version>/clew-server-<slug>-p<protocol>` (e.g.
/// `.../v0.1.3/clew-server-linux-x86_64-p6`), in the same release as the app
/// so a client fetches the server built with it.
fn base_url() -> String {
    std::env::var("CLEW_SERVER_DIST_URL")
        .unwrap_or_else(|_| "https://github.com/RutaTang/Clew/releases/download".to_string())
}

/// Platform slug from `uname -sm`, e.g. "Linux x86_64" -> "linux-x86_64".
///
/// `platform` comes from the remote's `uname` (untrusted), and the slug is joined
/// into a filesystem cache path and a download URL. Confine it to a plain token
/// (`[a-z0-9_-]`) so it can't traverse the path (`../`) or manipulate the URL
/// (`/`); anything else is rejected.
pub fn slug(platform: &str) -> Result<String, String> {
    let s = platform.trim().to_lowercase().replace(' ', "-");
    if s.is_empty()
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("unsupported remote platform {platform:?}"));
    }
    Ok(s)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let mut s = String::with_capacity(64);
    for b in hasher.finalize() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The digest out of a `.sha256` sidecar: the first 64-hex-char token, so both
/// a bare digest and `sha256sum` output ("digest  filename") parse.
fn parse_sha256(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .find(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The cached binary for this (version, protocol, slug) — but only when its
/// bytes still match the digest recorded at download time. A cache that fails
/// re-verification is removed so the caller re-downloads.
fn cached_if_valid(dir: &Path) -> Option<PathBuf> {
    let cache = dir.join("clew-server");
    let sumfile = dir.join("clew-server.sha256");
    let bytes = std::fs::read(&cache).ok()?;
    let expected = parse_sha256(&std::fs::read_to_string(&sumfile).ok()?)?;
    if hex_sha256(&bytes) == expected {
        return Some(cache);
    }
    // Tampered or corrupted: never deploy it. Drop both files.
    let _ = std::fs::remove_file(&cache);
    let _ = std::fs::remove_file(&sumfile);
    None
}

/// Verify `bytes` against `expected` and commit them to the cache dir
/// (write-then-rename, so a crash can't leave a half-written binary that a
/// later `cached_if_valid` would read alongside a complete sidecar).
fn commit_cache(dir: &Path, bytes: &[u8], expected: &str) -> Result<PathBuf, String> {
    let actual = hex_sha256(bytes);
    if actual != expected {
        return Err(format!(
            "clew-server download failed checksum verification (expected {expected}, got {actual})"
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let cache = dir.join("clew-server");
    // Uniquely named and exclusively created: two windows bootstrapping the
    // same platform's server at once would otherwise stage into one path and
    // rename a half-written mixture into place.
    let tmp = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let mut chosen = None;
        for _ in 0..64 {
            let n = N.fetch_add(1, Ordering::Relaxed);
            let candidate = dir.join(format!("clew-server.{pid}.{n}.tmp"));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(_) => {
                    chosen = Some(candidate);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        chosen.ok_or("could not create a temp file for the clew-server download")?
    };
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
    }
    std::fs::rename(&tmp, &cache).map_err(|e| e.to_string())?;
    // The sidecar lands last: until it exists, the cache never validates.
    std::fs::write(dir.join("clew-server.sha256"), format!("{expected}\n"))
        .map_err(|e| e.to_string())?;
    Ok(cache)
}

/// GET `url`, refusing bodies over `cap` bytes.
fn fetch(url: &str, cap: u64) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url).call().map_err(|e| format!("{url}: {e}"))?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > cap {
        return Err(format!("{url}: response exceeds {cap} bytes"));
    }
    Ok(bytes)
}

/// A local clew-server binary for `platform` at the client's `version` and
/// `protocol`, downloading it from the release host (and caching it under the
/// data dir) if not already present and valid. The protocol version is part
/// of the ASSET NAME, not just the cache key: a release that predates this
/// protocol simply has no matching asset, which surfaces as a clear error
/// here — never as a deployed server whose frames don't parse. Blocking; run
/// off the UI thread.
pub fn ensure_server_binary(
    platform: &str,
    version: &str,
    protocol: u32,
) -> Result<PathBuf, String> {
    let slug = slug(platform)?; // validated: safe to join into a path / URL
    let root = data_root().ok_or("no data directory")?;
    let dir = root
        .join("server-dist")
        .join(format!("v{version}-p{protocol}"))
        .join(&slug);
    if let Some(cache) = cached_if_valid(&dir) {
        return Ok(cache);
    }

    // Not cached (or failed re-verification): download the prebuilt binary
    // for this platform + version + protocol, and its digest.
    let url = format!("{}/v{version}/clew-server-{slug}-p{protocol}", base_url());
    let bytes = fetch(&url, MAX_SERVER_BYTES).map_err(|e| {
        format!(
            "no clew-server build for '{}' at v{version} protocol {protocol} ({e})",
            platform.trim()
        )
    })?;
    let sum_text = fetch(&format!("{url}.sha256"), 4096)
        .map_err(|e| format!("missing checksum for the clew-server build ({e})"))?;
    let expected = parse_sha256(&String::from_utf8_lossy(&sum_text))
        .ok_or("malformed clew-server checksum file")?;
    commit_cache(&dir, &bytes, &expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_accepts_known_platforms() {
        assert_eq!(slug("Linux x86_64").unwrap(), "linux-x86_64");
        assert_eq!(slug("Linux aarch64").unwrap(), "linux-aarch64");
        assert_eq!(slug("Darwin arm64").unwrap(), "darwin-arm64");
    }

    #[test]
    fn slug_rejects_traversal_and_url_manipulation() {
        // A malicious remote `uname` must not escape the cache path or the URL.
        assert!(slug("../../etc/passwd").is_err());
        assert!(slug("linux/../../evil").is_err());
        assert!(slug("..").is_err());
        assert!(slug("a/b").is_err());
        assert!(slug("a\\b").is_err());
        assert!(slug("http://evil").is_err());
        assert!(slug("a.b").is_err());
        assert!(slug("").is_err());
        assert!(slug("   ").is_err());
    }

    #[test]
    fn parse_sha256_accepts_bare_digest_and_sha256sum_output() {
        let d = "a".repeat(64);
        assert_eq!(parse_sha256(&d).as_deref(), Some(d.as_str()));
        assert_eq!(
            parse_sha256(&format!("{d}  clew-server-linux-x86_64-p6\n")).as_deref(),
            Some(d.as_str())
        );
        assert_eq!(parse_sha256("not a digest"), None);
        assert_eq!(parse_sha256(&"a".repeat(63)), None);
    }

    #[test]
    fn commit_verifies_and_cache_revalidates() {
        let dir = std::env::temp_dir().join("clew-server-dist-test");
        let _ = std::fs::remove_dir_all(&dir);
        let payload = b"#!/bin/sh\necho fake-server\n";
        let good = hex_sha256(payload);

        // A wrong digest refuses to commit anything.
        let bad = "0".repeat(64);
        assert!(commit_cache(&dir, payload, &bad).is_err());
        assert!(!dir.join("clew-server").exists());

        // The right digest commits, and the cache validates.
        let cache = commit_cache(&dir, payload, &good).unwrap();
        assert_eq!(cached_if_valid(&dir).as_deref(), Some(cache.as_path()));

        // Tampering with the cached binary invalidates it — and removes it,
        // so the next ensure re-downloads instead of deploying the tampered
        // bytes.
        std::fs::write(&cache, b"tampered").unwrap();
        assert_eq!(cached_if_valid(&dir), None);
        assert!(!dir.join("clew-server").exists());
    }
}
