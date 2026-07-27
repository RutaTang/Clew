//! Auto-update: find the latest clew release and fetch its assets.
//!
//! Pure logic plus HTTP, no UI and no macOS specifics (those live in the GUI
//! client). What gets downloaded is trusted by verifying Apple notarization and
//! our Developer ID signature before install, which the client does; this module
//! only locates and fetches. The release host is overridable via
//! `CLEW_UPDATE_API` (used by tests / self-hosting).

use std::io::{Read, Write};
use std::path::Path;

/// GitHub API base for the repo that hosts clew releases.
fn api_base() -> String {
    std::env::var("CLEW_UPDATE_API")
        .unwrap_or_else(|_| "https://api.github.com/repos/RutaTang/Clew".to_string())
}

/// GitHub wants a User-Agent on every request or it answers 403.
const USER_AGENT: &str = "clew-updater";

/// A semantic version `x.y.z` — the only shape clew's `vX.Y.Z` tags use.
///
/// Field order is major, minor, patch, so the derived `Ord` is exactly semver
/// precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Parse `1.2.3` or `v1.2.3`. Any pre-release / build suffix after the patch
    /// is ignored (clew tags are plain `vX.Y.Z`, but stay lenient).
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().trim_start_matches('v');
        let core = s.split(['-', '+']).next().unwrap_or(s);
        let mut it = core.split('.');
        let major = it.next()?.trim().parse().ok()?;
        let minor = it.next()?.trim().parse().ok()?;
        let patch = it.next()?.trim().parse().ok()?;
        Some(Version {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The newest published release: its version, notes (markdown), and the macOS
/// DMG download URL if one is attached.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    pub notes: String,
    pub dmg_url: Option<String>,
}

/// Query the newest published release. Blocking; run off the UI thread.
pub fn latest_release() -> Result<Release, String> {
    let url = format!("{}/releases/latest", api_base());
    let resp = ureq::get(&url)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("update check failed: {e}"))?;
    // ureq's `into_json` needs an extra feature; parse the body ourselves with
    // the serde_json already in this crate.
    let body = resp.into_string().map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;

    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or("release has no tag_name")?;
    let version = Version::parse(tag).ok_or_else(|| format!("unparseable tag {tag:?}"))?;
    let notes = json
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let dmg_url = json
        .get("assets")
        .and_then(|v| v.as_array())
        .and_then(|assets| {
            assets.iter().find_map(|a| {
                let name = a.get("name")?.as_str()?;
                if name.ends_with(".dmg") {
                    a.get("browser_download_url")?.as_str().map(str::to_string)
                } else {
                    None
                }
            })
        });

    Ok(Release {
        version,
        notes,
        dmg_url,
    })
}

/// Download `url` to `dest`, reporting `(bytes_so_far, total)` as it streams.
/// `total` is `None` when the server sends no `Content-Length`. Blocking; run off
/// the UI thread.
pub fn download_to(
    url: &str,
    dest: &Path,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let resp = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let total = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok());

    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 64 * 1024];
    let mut done: u64 = 0;
    on_progress(0, total);
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        on_progress(done, total);
    }
    file.flush().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Version;

    #[test]
    fn parses_plain_and_prefixed() {
        assert_eq!(Version::parse("1.2.3"), Version::parse("v1.2.3"));
        let v = Version::parse("v0.1.4").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (0, 1, 4));
    }

    #[test]
    fn ignores_prerelease_suffix() {
        assert_eq!(Version::parse("1.2.3-rc1"), Version::parse("1.2.3"));
        assert_eq!(Version::parse("1.2.3+build.5"), Version::parse("1.2.3"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Version::parse("").is_none());
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("x.y.z").is_none());
        assert!(Version::parse("nightly").is_none());
    }

    #[test]
    fn orders_by_semver_precedence() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("0.1.4") > v("0.1.3"));
        assert!(v("0.2.0") > v("0.1.9"));
        assert!(v("1.0.0") > v("0.9.9"));
        assert!(v("0.1.3") == v("v0.1.3"));
        assert!(!(v("0.1.3") > v("0.1.3")));
    }

    #[test]
    fn displays_without_v_prefix() {
        assert_eq!(Version::parse("v1.2.3").unwrap().to_string(), "1.2.3");
    }
}
