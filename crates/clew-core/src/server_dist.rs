//! Obtaining the clew-server binary for a remote's platform — automatically.
//!
//! A user never installs clew-server by hand. When connecting to a remote, the
//! client downloads the prebuilt binary for that remote's platform from the
//! release host (caching it locally), then deploys it over SSH. This mirrors how
//! VS Code ships its remote server. The release host is overridable via
//! `CLEW_SERVER_DIST_URL` (used by tests / self-hosting).

use std::io::Read;
use std::path::PathBuf;

use crate::lsp::store::data_root;

/// Where prebuilt clew-server binaries are published, one per platform slug:
/// `<base>/v<protocol>/clew-server-<slug>` (e.g. `.../v1/clew-server-linux-x86_64`).
fn base_url() -> String {
    std::env::var("CLEW_SERVER_DIST_URL")
        .unwrap_or_else(|_| "https://github.com/clew-reader/clew/releases/download".to_string())
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

/// A local clew-server binary for `platform`, downloading it from the release
/// host (and caching it under the data dir) if not already present. Keyed by the
/// protocol version, so a client update fetches a fresh binary. Blocking; run off
/// the UI thread.
pub fn ensure_server_binary(platform: &str) -> Result<PathBuf, String> {
    let slug = slug(platform)?; // validated: safe to join into a path / URL
    let version = clew_protocol::PROTOCOL_VERSION;
    let root = data_root().ok_or("no data directory")?;
    let cache = root
        .join("server-dist")
        .join(format!("v{version}"))
        .join(&slug)
        .join("clew-server");
    if cache.exists() {
        return Ok(cache);
    }

    // Not cached: download the prebuilt binary for this platform.
    let url = format!("{}/v{version}/clew-server-{slug}", base_url());
    let resp = ureq::get(&url)
        .call()
        .map_err(|e| format!("no clew-server for '{}' ({e})", platform.trim()))?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;

    if let Some(dir) = cache.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&cache, &bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755));
    }
    Ok(cache)
}

#[cfg(test)]
mod tests {
    use super::slug;

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
}
