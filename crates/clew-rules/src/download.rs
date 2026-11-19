//! Where an update may be downloaded from: `validate_dmg_url` in
//! `src/backend/updater.rs` reads a URL with the `url` crate, as the download
//! will send it, and asks [`release_asset`] of its host and path.
//!
//! That a download accepted is from the releases' host and under their
//! downloads is proved in `verification/Clew/Update.lean`, and the Lean
//! Aeneas makes of this source is proved to be that model.

use crate::text::{same_bytes, starts_with};

/// Whether a URL whose host is `host` and whose path (as the `url` crate
/// resolves it: no `.` or `..` pieces, `%2e` decoded where it is one) is
/// `path` names an asset of the releases: the host is `releases_host`
/// exactly, and the path starts with `downloads`.
pub fn release_asset(host: &[u8], path: &[u8], releases_host: &[u8], downloads: &[u8]) -> bool {
    same_bytes(host, releases_host) && starts_with(path, downloads)
}

#[cfg(test)]
mod tests {
    use super::release_asset;

    #[test]
    fn only_the_releases_host_and_downloads() {
        let host = b"github.com";
        let downloads = b"/RutaTang/Clew/releases/download/";
        let ok = |h: &str, p: &str| release_asset(h.as_bytes(), p.as_bytes(), host, downloads);
        assert!(ok(
            "github.com",
            "/RutaTang/Clew/releases/download/v1/Clew.dmg"
        ));
        assert!(!ok(
            "github.com.evil",
            "/RutaTang/Clew/releases/download/v1/Clew.dmg"
        ));
        assert!(!ok("githu", "/RutaTang/Clew/releases/download/v1/Clew.dmg"));
        assert!(!ok("github.com", "/RutaTang/Clew/releases/downloadX/a"));
        assert!(!ok("github.com", "/RutaTang/Clew/releases/"));
        assert!(!ok(
            "github.com",
            "/Other/Clew/releases/download/v1/Clew.dmg"
        ));
    }
}
