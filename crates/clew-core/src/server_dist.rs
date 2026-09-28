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
//! deserialize.
//!
//! # What the bytes are checked against
//!
//! This binary receives the user's API keys on the remote, so where its digest
//! comes from matters:
//!
//! - **A release build** embeds the SHA-256 of every server asset of its own
//!   release at compile time (`CLEW_SERVER_DIGESTS`, set by the release
//!   workflow once the server jobs have built; format in
//!   [`parse_digest_list`]). The download AND every later reuse of the cache
//!   are checked against that list — which travels inside the signed,
//!   notarized app — so neither the release host, nor anything that can
//!   write clew's cache directory, can substitute other bytes. An asset the
//!   list does not name is refused.
//! - **A build without the list** (a developer build) falls back to the
//!   `.sha256` sidecar published beside each asset. That guards against a
//!   corrupted transfer, and nothing more: the sidecar comes from the same
//!   host, over the same TLS connection, as the binary, and the cached copy
//!   beside the cached binary proves nothing about local tampering.
//!
//! Either way the VERIFIED BYTES are what the caller gets ([`server_binary`]),
//! so the deployer streams exactly what was checked instead of re-reading a
//! path that could have changed since.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::lsp::store::data_root;

/// Hard cap on a downloaded server binary (the static musl build is ~20 MB).
/// A response larger than this is broken or hostile; reading it to the end
/// would balloon memory before any other check can reject it.
const MAX_SERVER_BYTES: u64 = 100 * 1024 * 1024;

/// Wall-clock budget for downloading one server binary. Public so the
/// client's own bound on a whole [`server_binary`] call can be derived from it
/// (it must be LONGER, or it fires first and abandons the download running).
pub const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// The digests of this build's own release, embedded by the release workflow
/// (see the module docs). `None` in a build made without it.
const EMBEDDED_DIGESTS: Option<&str> = option_env!("CLEW_SERVER_DIGESTS");

/// How long a cached build of ANOTHER version or protocol survives unused.
const STALE_DIST_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Most `server-dist/` entries one sweep looks at.
const MAX_SWEPT_DIST_ENTRIES: usize = 256;

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

/// The release asset name for a platform slug and protocol.
pub fn asset_name(slug: &str, protocol: u32) -> String {
    format!("clew-server-{slug}-p{protocol}")
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

fn is_sha256_hex(t: &str) -> bool {
    t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit())
}

/// The digest out of a `.sha256` sidecar: the first 64-hex-char token, so both
/// a bare digest and `sha256sum` output ("digest  filename") parse.
fn parse_sha256(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .find(|t| is_sha256_hex(t))
}

/// Parse the embedded digest list into `asset name -> sha256`.
///
/// Entries are separated by newlines, `;` or `,`, and each is either
/// `sha256sum` output — `<64 hex> <asset>` or `<64 hex> *<asset>` (any
/// whitespace between) — or `<asset>=<64 hex>`. Anything else is ignored, so
/// a list assembled by concatenating the release's `.sha256` sidecars works
/// as-is.
pub fn parse_digest_list(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for entry in text.split(['\n', ';', ',']).map(str::trim) {
        if let Some((name, digest)) = entry.split_once('=') {
            let (name, digest) = (name.trim(), digest.trim());
            if is_sha256_hex(digest) && !name.is_empty() {
                out.insert(name.to_string(), digest.to_ascii_lowercase());
            }
            continue;
        }
        let mut parts = entry.split_whitespace();
        if let (Some(digest), Some(name), None) = (parts.next(), parts.next(), parts.next())
            && is_sha256_hex(digest)
        {
            let name = name.trim_start_matches('*');
            if !name.is_empty() {
                out.insert(name.to_string(), digest.to_ascii_lowercase());
            }
        }
    }
    out
}

/// The digest `asset` must have, per `embedded` (the build-time list):
/// `Ok(None)` when no list was embedded (fall back to the sidecar), an error
/// when there is one and it does not name this asset.
fn embedded_digest(embedded: Option<&str>, asset: &str) -> Result<Option<String>, String> {
    let Some(list) = embedded else {
        return Ok(None);
    };
    parse_digest_list(list)
        .remove(asset)
        .map(Some)
        .ok_or_else(|| format!("this build of clew ships no clew-server named {asset}"))
}

/// A clew-server binary whose bytes were checked (see the module docs).
#[derive(Debug, Clone)]
pub struct VerifiedServer {
    /// The binary itself — what must be deployed.
    pub bytes: Vec<u8>,
    /// Its SHA-256, as it was verified.
    pub sha256: String,
}

/// The clew-server binary for `platform` at the client's `version` and
/// `protocol`: from the cache if it still verifies, otherwise downloaded from
/// the release host, verified, and cached. The protocol version is part of
/// the ASSET NAME, not just the cache key: a release that predates this
/// protocol simply has no matching asset, which surfaces as a clear error
/// here — never as a deployed server whose frames don't parse. Blocking; run
/// off the UI thread.
pub fn server_binary(
    platform: &str,
    version: &str,
    protocol: u32,
) -> Result<VerifiedServer, String> {
    let slug = slug(platform)?; // validated: safe to join into a path / URL
    let asset = asset_name(&slug, protocol);
    let pinned = embedded_digest(EMBEDDED_DIGESTS, &asset)?;
    let dist = dist_root()?;
    let release = format!("v{version}-p{protocol}");
    let dir = dist.join(&release).join(&slug);

    if let Some(cached) = cached_if_valid(&dir, pinned.as_deref()) {
        touch(&dist.join(&release));
        sweep(&dist, &release);
        return Ok(cached);
    }

    // Not cached (or failed re-verification): download the prebuilt binary
    // for this platform + version + protocol, and verify it.
    let url = format!("{}/v{version}/{asset}", base_url());
    let bytes = crate::net::get(
        &url,
        &[],
        crate::net::Limits {
            max_bytes: MAX_SERVER_BYTES,
            deadline: DOWNLOAD_DEADLINE,
        },
    )
    .map_err(|e| {
        format!(
            "no clew-server build for '{}' at v{version} protocol {protocol} ({e})",
            platform.trim()
        )
    })?;
    let expected = match pinned {
        Some(digest) => digest,
        None => {
            // No build-time list: the sidecar is all there is (integrity
            // against a corrupted transfer only — see the module docs).
            let sum = crate::net::get(
                &format!("{url}.sha256"),
                &[],
                crate::net::Limits {
                    max_bytes: 4096,
                    deadline: Duration::from_secs(60),
                },
            )
            .map_err(|e| format!("missing checksum for the clew-server build ({e})"))?;
            parse_sha256(&String::from_utf8_lossy(&sum))
                .ok_or("malformed clew-server checksum file")?
        }
    };
    let verified = verify(bytes, &expected)?;
    // A cache that cannot be written costs the next connect a download; it
    // does not stop this one.
    let _ = commit_cache(&dir, &verified);
    sweep(&dist, &release);
    Ok(verified)
}

/// `<data>/server-dist`, private (0700).
fn dist_root() -> Result<PathBuf, String> {
    let root = data_root().ok_or("no data directory")?;
    crate::derived::ensure_private_dir(&root).map_err(|e| e.to_string())?;
    let dist = root.join("server-dist");
    crate::derived::ensure_private_dir(&dist).map_err(|e| e.to_string())?;
    Ok(dist)
}

fn verify(bytes: Vec<u8>, expected: &str) -> Result<VerifiedServer, String> {
    let actual = hex_sha256(&bytes);
    if actual != expected.to_ascii_lowercase() {
        return Err(format!(
            "clew-server download failed checksum verification (expected {expected}, got {actual})"
        ));
    }
    Ok(VerifiedServer {
        bytes,
        sha256: actual,
    })
}

/// The cached binary for this (version, protocol, slug) — but only when its
/// bytes still match: the build-time digest when there is one, else the
/// sidecar recorded at download time. A cache that fails re-verification is
/// removed so the caller re-downloads.
fn cached_if_valid(dir: &Path, pinned: Option<&str>) -> Option<VerifiedServer> {
    use std::io::Read;
    let cache = dir.join("clew-server");
    let sumfile = dir.join("clew-server.sha256");
    let f = crate::statefile::open_plain(&cache)?;
    let mut bytes = Vec::new();
    f.take(MAX_SERVER_BYTES + 1).read_to_end(&mut bytes).ok()?;
    let expected = match pinned {
        Some(digest) => Some(digest.to_ascii_lowercase()),
        None => crate::statefile::read_capped(&sumfile, 4096).and_then(|t| parse_sha256(&t)),
    };
    if bytes.len() as u64 <= MAX_SERVER_BYTES
        && let Some(expected) = expected
        && let Ok(verified) = verify(bytes, &expected)
    {
        return Some(verified);
    }
    // Tampered, corrupted or unverifiable: never deploy it. Drop both files.
    let _ = std::fs::remove_file(&cache);
    let _ = std::fs::remove_file(&sumfile);
    None
}

/// Commit verified bytes to the cache dir: the binary through a uniquely named
/// temp file (synced, then renamed, so a crash can't leave a half-written
/// binary that a later `cached_if_valid` would read alongside a complete
/// sidecar), then the sidecar — until it exists, the cache never validates.
fn commit_cache(dir: &Path, verified: &VerifiedServer) -> Result<PathBuf, String> {
    crate::derived::ensure_private_dir(dir).map_err(|e| e.to_string())?;
    let cache = dir.join("clew-server");
    crate::statefile::write_atomic(&cache, &verified.bytes).map_err(|e| e.to_string())?;
    crate::statefile::write_atomic(
        &dir.join("clew-server.sha256"),
        format!("{}\n", verified.sha256).as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    Ok(cache)
}

/// Mark a release directory as used, so the sweep keeps it.
fn touch(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.set_modified(std::time::SystemTime::now());
    }
}

/// Bounded housekeeping for `<data>/server-dist/`: every client version and
/// protocol used to leave its ~20 MB builds behind forever. Entries other than
/// `keep` that nothing has used for [`STALE_DIST_AGE`] are removed; a version
/// in use by another clew on this machine stays (its every use refreshes it),
/// and one swept anyway is simply downloaded again.
fn sweep(dist: &Path, keep: &str) {
    sweep_older_than(dist, keep, std::time::SystemTime::now(), STALE_DIST_AGE);
}

fn sweep_older_than(dist: &Path, keep: &str, now: std::time::SystemTime, age: Duration) {
    let Ok(entries) = std::fs::read_dir(dist) else {
        return;
    };
    for entry in entries.flatten().take(MAX_SWEPT_DIST_ENTRIES) {
        if entry.file_name().to_str() == Some(keep) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|d| d > age);
        if !old {
            continue;
        }
        let _ = if meta.is_dir() {
            std::fs::remove_dir_all(entry.path())
        } else {
            std::fs::remove_file(entry.path())
        };
    }
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

    /// The build-time list in every shape the release workflow might produce:
    /// concatenated `sha256sum` output (text or binary mode), or `name=digest`
    /// pairs separated by `;`/`,`/newlines.
    #[test]
    fn the_embedded_digest_list_parses_in_every_documented_shape() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let list = format!(
            "{a}  clew-server-linux-x86_64-p11\n{b} *clew-server-linux-aarch64-p11\n\
             clew-server-linux-riscv64-p11={a}; garbage line\n"
        );
        let parsed = parse_digest_list(&list);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed["clew-server-linux-x86_64-p11"], a);
        assert_eq!(
            parsed["clew-server-linux-aarch64-p11"],
            b.to_ascii_lowercase()
        );
        assert_eq!(parsed["clew-server-linux-riscv64-p11"], a);

        // With a list, the asset must be in it; without one, the sidecar is
        // the fallback.
        assert_eq!(
            embedded_digest(Some(&list), "clew-server-linux-x86_64-p11").unwrap(),
            Some(a.clone())
        );
        assert!(embedded_digest(Some(&list), "clew-server-darwin-arm64-p11").is_err());
        assert_eq!(embedded_digest(None, "anything").unwrap(), None);
    }

    fn verified(payload: &[u8]) -> VerifiedServer {
        VerifiedServer {
            bytes: payload.to_vec(),
            sha256: hex_sha256(payload),
        }
    }

    #[test]
    fn commit_verifies_and_cache_revalidates() {
        let scratch = crate::testutil::TempDir::new("server-dist-commit");
        let dir = scratch.join("cache");
        let payload = b"#!/bin/sh\necho fake-server\n";
        let good = hex_sha256(payload);

        // A wrong digest refuses the bytes outright.
        assert!(verify(payload.to_vec(), &"0".repeat(64)).is_err());

        // The right digest commits, and the cache validates — handing back
        // the verified BYTES, not a path to re-read.
        let cache = commit_cache(&dir, &verified(payload)).unwrap();
        let back = cached_if_valid(&dir, None).expect("valid against its sidecar");
        assert_eq!(back.bytes, payload);
        assert_eq!(back.sha256, good);

        // Tampering with the cached binary invalidates it — and removes it,
        // so the next `server_binary` downloads again instead of deploying
        // the tampered bytes.
        std::fs::write(&cache, b"tampered").unwrap();
        assert!(cached_if_valid(&dir, None).is_none());
        assert!(!dir.join("clew-server").exists());
    }

    /// With a build-time digest, the sidecar beside the cache is worthless to
    /// an attacker: rewriting the binary AND its sidecar together is exactly
    /// the local tampering the sidecar alone could never detect.
    #[test]
    fn a_pinned_digest_catches_a_consistently_rewritten_cache() {
        let scratch = crate::testutil::TempDir::new("server-dist-pinned");
        let dir = scratch.join("cache");
        let genuine = b"genuine server";
        commit_cache(&dir, &verified(genuine)).unwrap();
        let pinned = hex_sha256(genuine);
        assert!(cached_if_valid(&dir, Some(&pinned)).is_some());

        // Swap both files for a matching pair.
        let evil = b"evil server";
        commit_cache(&dir, &verified(evil)).unwrap();
        assert!(
            cached_if_valid(&dir, None).is_some(),
            "the sidecar alone is fooled"
        );
        assert!(
            cached_if_valid(&dir, Some(&pinned)).is_none(),
            "the embedded digest is not"
        );
        assert!(!dir.join("clew-server").exists(), "and the fake is dropped");
    }

    /// Builds of other versions are swept once unused for a week; the one in
    /// use, and recent ones, stay.
    #[test]
    fn stale_builds_of_other_versions_are_swept() {
        let dist = crate::testutil::TempDir::new("server-dist-sweep");
        for v in ["v1.0.0-p10", "v1.1.0-p11", "v1.2.0-p12"] {
            std::fs::create_dir_all(dist.join(v).join("linux-x86_64")).unwrap();
        }
        let old = std::time::SystemTime::now() - Duration::from_secs(30 * 24 * 60 * 60);
        for v in ["v1.0.0-p10", "v1.2.0-p12"] {
            std::fs::File::open(dist.join(v))
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        sweep_older_than(
            &dist,
            "v1.2.0-p12",
            std::time::SystemTime::now(),
            STALE_DIST_AGE,
        );
        assert!(!dist.join("v1.0.0-p10").exists(), "stale: swept");
        assert!(dist.join("v1.1.0-p11").exists(), "recent: kept");
        assert!(
            dist.join("v1.2.0-p12").exists(),
            "current: kept however old"
        );
    }

    /// End to end against a loopback "release host": the download is verified
    /// against its sidecar, cached privately, and served from the cache next
    /// time — and a corrupted transfer (bytes that do not match their
    /// checksum) is refused and never cached.
    #[test]
    fn server_binary_downloads_verifies_and_caches() {
        use std::io::Write;
        // `CLEW_DATA_DIR` (and the env lock) for the whole test; declared
        // first, so the URL override below is restored before it goes.
        let data = crate::testutil::DataDir::new("server-dist-e2e");
        let payload = b"\x7fELF fake server".to_vec();
        let sum = format!("{}  clew-server-linux-x86_64-p99\n", hex_sha256(&payload));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body_for = move |path: &str| -> Vec<u8> {
            if path.ends_with(".sha256") {
                sum.clone().into_bytes()
            } else if path.starts_with("/v9.9.8/") {
                // The same checksum, other bytes: a corrupted transfer.
                b"\x7fELF fake servxr".to_vec()
            } else {
                payload.clone()
            }
        };
        std::thread::spawn(move || {
            // Two requests per download (binary, sidecar), two downloads.
            for stream in listener.incoming().take(4) {
                let Ok(mut stream) = stream else { continue };
                // The whole request before the answer (see `testutil`).
                let request = crate::testutil::read_http(&mut stream);
                let body = body_for(request.path());
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        let _url =
            crate::testutil::EnvVars::new().set("CLEW_SERVER_DIST_URL", format!("http://{addr}"));

        let first = server_binary("Linux x86_64", "9.9.9", 99);
        if EMBEDDED_DIGESTS.is_some() {
            // A release build pins its own assets, and this fake is not one.
            assert!(first.is_err());
            return;
        }
        let first = first.expect("downloaded and verified");
        assert_eq!(first.bytes, b"\x7fELF fake server");
        // Served from the cache now: no request reaches the host for it.
        let second = server_binary("Linux x86_64", "9.9.9", 99).expect("served from the cache");
        assert_eq!(second.bytes, first.bytes);

        let corrupted = server_binary("Linux x86_64", "9.9.8", 99)
            .expect_err("bytes that do not match their checksum must be refused");
        assert!(corrupted.contains("checksum"), "{corrupted}");
        assert!(
            !data
                .join("server-dist/v9.9.8-p99/linux-x86_64/clew-server")
                .exists(),
            "and nothing refused is cached"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(data.join("server-dist"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "the cache is private");
        }
    }
}
