//! Auto-update: find the latest clew release and fetch its assets.
//!
//! Pure logic plus HTTP, no UI and no macOS specifics (those live in the GUI
//! client). Nothing here validates what it fetches: a downloaded image is
//! untrusted bytes until the client's installer checks the bundle inside it
//! against a requirement pinning an Apple anchor and the running app's own
//! signing team (`src/macos/install.rs`). That requirement is what establishes
//! provenance; the installer also requires Apple notarization, but only as a
//! second gate, and one whose strictness is coupled to the release workflow
//! rather than fixed. The release host is overridable via `CLEW_UPDATE_API`
//! (used by tests / self-hosting), which is another reason no trust can rest on
//! where the bytes came from.
//!
//! What this module does guarantee is the transport (HTTPS, timeouts, byte
//! caps — see [`crate::net`]), that the image offered is built for the
//! running architecture, and the rule that an update never moves BACKWARDS —
//! which the caller applies twice: a release is offered only when its
//! [`Release::version`] is newer than the running one (the updater's
//! "check for updates" compares them), and [`ensure_upgrade`] repeats the
//! check at install time for the version the downloaded bundle actually
//! declares, since a tag alone says nothing about the bytes behind it.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// GitHub API base for the repo that hosts clew releases.
fn api_base() -> String {
    std::env::var("CLEW_UPDATE_API")
        .unwrap_or_else(|_| "https://api.github.com/repos/RutaTang/Clew".to_string())
}

/// Largest release-metadata document accepted (GitHub's are a few KB).
const MAX_RELEASE_JSON_BYTES: u64 = 4 * 1024 * 1024;

/// Largest update image accepted. A clew DMG is tens of MB.
pub const MAX_UPDATE_BYTES: u64 = 512 * 1024 * 1024;

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

/// The newest published release: its version, notes (markdown), and the
/// download URL of the macOS DMG built for THIS machine's architecture, if
/// the release has one (see [`pick_dmg`]).
#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    pub notes: String,
    pub dmg_url: Option<String>,
}

/// Query the newest published release. Blocking; run off the UI thread.
pub fn latest_release() -> Result<Release, String> {
    let url = format!("{}/releases/latest", api_base());
    let body = crate::net::get(
        &url,
        &[("Accept", "application/vnd.github+json")],
        crate::net::Limits {
            max_bytes: MAX_RELEASE_JSON_BYTES,
            deadline: Duration::from_secs(60),
        },
    )
    .map_err(|e| format!("update check failed: {e}"))?;
    parse_release(&body, std::env::consts::ARCH)
}

/// The installer's no-downgrade check: `candidate` is the version the
/// DOWNLOADED BUNDLE declares (its `CFBundleShortVersionString`), and it must
/// parse and be strictly newer than `running`. The release tag is not enough —
/// it is a label on the release page, and says nothing about which build the
/// image actually contains.
pub fn ensure_upgrade(running: Version, candidate: &str) -> Result<Version, String> {
    let v = Version::parse(candidate)
        .ok_or_else(|| format!("the update declares an unreadable version {candidate:?}"))?;
    if v <= running {
        return Err(format!(
            "the update contains clew {v}, which is not newer than the running {running} — \
             refusing to install it"
        ));
    }
    Ok(v)
}

fn parse_release(body: &[u8], arch: &str) -> Result<Release, String> {
    let json: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
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
    let assets = json
        .get("assets")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    Ok(Release {
        version,
        notes,
        dmg_url: pick_dmg(assets, arch),
    })
}

/// The download URL of the DMG built for `arch` (`std::env::consts::ARCH`):
/// one whose name carries this architecture (`arm64`/`aarch64` for Apple
/// silicon, `x86_64`/`x64`/`intel` for Intel), else a `universal` one.
///
/// Picking the first `.dmg` of the release, as this used to, would hand an
/// Intel Mac an Apple-silicon-only image the moment a release shipped both —
/// and the swapped-in app would not launch. A DMG that names no architecture
/// is not guessed at either: `None` sends the user to the release page.
pub fn pick_dmg(assets: &[serde_json::Value], arch: &str) -> Option<String> {
    let wanted: &[&str] = match arch {
        "aarch64" => &["arm64", "aarch64", "applesilicon"],
        "x86_64" => &["x86_64", "x64", "amd64", "intel"],
        _ => &[],
    };
    let dmgs: Vec<(Vec<String>, &str)> = assets
        .iter()
        .filter_map(|a| {
            let name = a.get("name")?.as_str()?.to_ascii_lowercase();
            let url = a.get("browser_download_url")?.as_str()?;
            let stem = name.strip_suffix(".dmg")?;
            let tokens = stem
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect();
            Some((tokens, url))
        })
        .collect();
    let has = |tokens: &[String], set: &[&str]| tokens.iter().any(|t| set.contains(&t.as_str()));
    dmgs.iter()
        .find(|(tokens, _)| has(tokens, wanted))
        .or_else(|| dmgs.iter().find(|(tokens, _)| has(tokens, &["universal"])))
        .map(|(_, url)| (*url).to_string())
}

/// Download `url` to `dest`, reporting `(bytes_so_far, total)` as it streams.
/// `total` is `None` when the server sends no `Content-Length`. HTTPS only,
/// with connect/read timeouts, and capped at [`MAX_UPDATE_BYTES`]; abandoned
/// the moment `cancel` is set (see [`crate::net::download`]). Blocking; run
/// off the UI thread.
pub fn download_to(
    url: &str,
    dest: &Path,
    cancel: &AtomicBool,
    on_progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    download_to_capped(url, dest, MAX_UPDATE_BYTES, cancel, on_progress)
}

/// [`download_to`] with an explicit byte cap.
pub fn download_to_capped(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    cancel: &AtomicBool,
    on_progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    // Truncating rather than following whatever is at `dest`: on unix the
    // create refuses to open through a symlink, so a planted link fails the
    // download instead of redirecting it.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = opts.open(dest).map_err(|e| e.to_string())?;
    crate::net::download(
        url,
        &[],
        crate::net::Limits {
            max_bytes,
            // Slow links are fine (every read has its own stall timeout);
            // this only stops a transfer that trickles forever.
            deadline: Duration::from_secs(2 * 60 * 60),
        },
        cancel,
        &mut file,
        on_progress,
    )
    .map_err(|e| format!("download failed: {e}"))?;
    file.sync_all().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cancel flag nothing sets.
    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

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
        assert!((v("0.1.3") <= v("0.1.3")));
    }

    #[test]
    fn displays_without_v_prefix() {
        assert_eq!(Version::parse("v1.2.3").unwrap().to_string(), "1.2.3");
    }

    fn asset(name: &str) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "browser_download_url": format!("https://example.invalid/{name}"),
        })
    }

    /// The first `.dmg` is not necessarily this machine's: a release with
    /// both architectures must give each Mac its own, and an image that names
    /// no architecture is not guessed at.
    #[test]
    fn the_dmg_is_picked_by_architecture() {
        let both = [
            asset("Clew-0.2.0-x86_64.dmg"),
            asset("Clew-0.2.0-arm64.dmg"),
            asset("clew-server-linux-x86_64-p11"),
        ];
        assert_eq!(
            pick_dmg(&both, "aarch64").as_deref(),
            Some("https://example.invalid/Clew-0.2.0-arm64.dmg")
        );
        assert_eq!(
            pick_dmg(&both, "x86_64").as_deref(),
            Some("https://example.invalid/Clew-0.2.0-x86_64.dmg")
        );

        let arm_only = [asset("Clew-0.2.0-arm64.dmg")];
        assert_eq!(pick_dmg(&arm_only, "x86_64"), None, "no Intel image");

        let universal = [asset("Clew-0.2.0-universal.dmg")];
        assert!(pick_dmg(&universal, "x86_64").is_some());
        assert!(pick_dmg(&universal, "aarch64").is_some());

        let unnamed = [asset("Clew-0.2.0.dmg")];
        assert_eq!(pick_dmg(&unnamed, "aarch64"), None, "not guessed at");
        // Architecture tokens are whole words, not substrings.
        let tricky = [asset("Clew-0.2.0-x64dbg.dmg")];
        assert_eq!(pick_dmg(&tricky, "x86_64"), None);
    }

    #[test]
    fn a_release_parses_with_its_architectures_dmg() {
        let body = serde_json::json!({
            "tag_name": "v0.3.1",
            "body": "notes",
            "assets": [asset("Clew-0.3.1-arm64.dmg")],
        })
        .to_string();
        let r = parse_release(body.as_bytes(), "aarch64").unwrap();
        assert_eq!(r.version, Version::parse("0.3.1").unwrap());
        assert_eq!(r.notes, "notes");
        assert!(r.dmg_url.unwrap().ends_with("arm64.dmg"));
        assert!(parse_release(b"{}", "aarch64").is_err());
    }

    /// Never backwards, and never sideways: the bundle's own version must be
    /// strictly newer than what is running.
    #[test]
    fn downgrades_and_reinstalls_are_refused() {
        let running = Version::parse("1.4.2").unwrap();
        assert!(ensure_upgrade(running, "1.4.3").is_ok());
        assert!(ensure_upgrade(running, "2.0.0").is_ok());
        assert!(ensure_upgrade(running, "1.4.2").is_err(), "same version");
        let err = ensure_upgrade(running, "1.3.9").unwrap_err();
        assert!(err.contains("not newer"), "{err}");
        assert!(ensure_upgrade(running, "garbage").is_err());
    }

    /// The download is capped while streaming, and a plain-http URL to a
    /// real host is refused before any byte is written.
    #[test]
    fn downloads_are_https_only_and_capped() {
        use std::io::Write;
        let dir = crate::testutil::TempDir::new("update-download");

        let err = download_to(
            "http://example.com/x.dmg",
            &dir.join("a.dmg"),
            &never(),
            |_, _| {},
        )
        .unwrap_err();
        assert!(err.contains("non-HTTPS"), "{err}");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                crate::testutil::read_http_request(&mut s);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
                let _ = s.write_all(&[7u8; 4096]);
            }
        });
        let err = download_to_capped(
            &format!("http://{addr}/big.dmg"),
            &dir.join("b.dmg"),
            1024,
            &never(),
            |_, _| {},
        )
        .unwrap_err();
        assert!(err.contains("limit"), "{err}");
    }

    /// A fake release host on loopback: answers each connection in turn with
    /// the next of `responses`, once the whole request is in, and reports each
    /// request's head (request line and headers).
    fn serve(responses: Vec<String>) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (heads, seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = heads.send(crate::testutil::read_http(&mut stream).head);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// Gap 1 (tests findings): the update check asks the configured release
    /// host (`CLEW_UPDATE_API`) for its latest release — as GitHub's API
    /// wants to be asked, with a user agent — and reads this machine's image
    /// out of the answer; a host that fails, or answers with more than a
    /// release document can hold, is an error, never a release.
    #[test]
    fn the_latest_release_comes_from_the_configured_host() {
        let release = serde_json::json!({
            "tag_name": "v9.8.7",
            "body": "what changed",
            "assets": [asset("Clew-9.8.7-arm64.dmg"), asset("Clew-9.8.7-x86_64.dmg")],
        })
        .to_string();
        let (base, heads) = serve(vec![
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{release}",
                release.len()
            ),
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".into(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_RELEASE_JSON_BYTES + 1
            ),
        ]);
        let _env =
            crate::testutil::EnvVars::new().set("CLEW_UPDATE_API", format!("{base}/repos/o/r"));

        let latest = latest_release().unwrap();
        assert_eq!(latest.version, Version::parse("9.8.7").unwrap());
        assert_eq!(latest.notes, "what changed");
        let expected = match std::env::consts::ARCH {
            "aarch64" => Some("https://example.invalid/Clew-9.8.7-arm64.dmg"),
            "x86_64" => Some("https://example.invalid/Clew-9.8.7-x86_64.dmg"),
            _ => None,
        };
        assert_eq!(latest.dmg_url.as_deref(), expected);
        let head = heads.recv().unwrap().to_ascii_lowercase();
        assert!(
            head.starts_with("get /repos/o/r/releases/latest http/1.1\r\n"),
            "{head}"
        );
        let lines: Vec<&str> = head.lines().collect();
        assert!(
            lines.contains(&"accept: application/vnd.github+json"),
            "{head}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("user-agent: clew/")),
            "{head}"
        );

        let err = latest_release().unwrap_err();
        assert!(
            err.starts_with("update check failed") && err.contains("404"),
            "{err}"
        );
        let err = latest_release().unwrap_err();
        assert!(err.contains("byte limit"), "{err}");
    }

    /// Gap 1 (tests findings): a body that ends before its declared length —
    /// a dropped connection mid-image — is a failed download, not a short
    /// image handed on to the installer.
    #[test]
    fn a_truncated_download_is_a_failure() {
        let dir = crate::testutil::TempDir::new("update-truncated");
        let (base, _heads) = serve(vec![format!(
            "HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n{}",
            "x".repeat(100)
        )]);
        let mut progress = Vec::new();
        let err = download_to(
            &format!("{base}/Clew.dmg"),
            &dir.join("c.dmg"),
            &never(),
            |done, total| progress.push((done, total)),
        )
        .unwrap_err();
        assert!(err.starts_with("download failed"), "{err}");
        assert!(err.contains("closed before all bytes were read"), "{err}");
        assert_eq!(progress.last(), Some(&(100, Some(4096))), "{progress:?}");
    }

    /// A download stops the moment it is cancelled — here mid-body, with the
    /// server gone quiet, where it used to sit out the stall timeout (and
    /// the updater's download could not be stopped at all).
    #[test]
    fn a_cancelled_download_stops_at_once() {
        use std::io::Write;
        use std::sync::atomic::Ordering;
        let dir = crate::testutil::TempDir::new("update-cancel");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                crate::testutil::read_http_request(&mut s);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\nabc");
                // Quiet from here: the client ends it, or this bound does.
                let _ = s.set_read_timeout(Some(Duration::from_secs(20)));
                let _ = std::io::Read::read(&mut s, &mut [0u8; 1]);
            }
        });
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let started = std::time::Instant::now();
        let mut seen = 0;
        let err = download_to(
            &format!("http://{addr}/Clew.dmg"),
            &dir.join("d.dmg"),
            &cancel,
            |done, _| {
                seen = done;
                if done > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
        )
        .unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert_eq!(seen, 3);
        assert!(started.elapsed() < Duration::from_secs(5), "{err}");
    }
}
