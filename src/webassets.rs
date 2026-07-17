//! Provisions the JS libraries that render math and mermaid diagrams for the
//! explanation modal into clew's global data dir.
//!
//! Downloads are checksum-verified over HTTPS — the same trust model as the LSP
//! server store — and cached, so the `clew-view --export` helper drives only
//! local, verified code (no CDN) and works offline after the first fetch.
//! MathJax runs in SVG mode, which embeds glyph paths and so needs no separate
//! font files; the produced SVGs are shown inline via iced's `svg` widget.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

struct Asset {
    file: &'static str,
    url: &'static str,
    sha256: &'static str,
}

const ASSETS: &[Asset] = &[
    Asset {
        file: "tex-svg.js",
        url: "https://cdn.jsdelivr.net/npm/mathjax@3.2.2/es5/tex-svg.js",
        sha256: "d4295dc33744836935c1399feece5159577b34c5c8ffb9f1c6324cd82e03a882",
    },
    Asset {
        file: "mermaid.min.js",
        url: "https://cdn.jsdelivr.net/npm/mermaid@10.9.1/dist/mermaid.min.js",
        sha256: "61b335a46df05a7ce1c98378f60e5f3e77a7fb608a1056997e8a649304a936d6",
    },
];

fn dir() -> Option<PathBuf> {
    Some(crate::lsp::store::data_root()?.join("webassets"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest_matches(path: &Path, sha256: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    hex(&Sha256::digest(&bytes)) == sha256
}

fn download(asset: &Asset, dest: &Path) -> Result<(), String> {
    if !asset.url.starts_with("https://") {
        return Err("refusing non-HTTPS asset URL".into());
    }
    let resp = ureq::get(asset.url)
        .call()
        .map_err(|e| format!("download {}: {e}", asset.file))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| format!("read {}: {e}", asset.file))?;
    let got = hex(&Sha256::digest(&buf));
    if got != asset.sha256 {
        return Err(format!("{}: checksum mismatch (got {got})", asset.file));
    }
    let tmp = dest.with_extension("tmp");
    std::fs::write(&tmp, &buf).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
    Ok(())
}

/// Ensure every render asset is present and verified, downloading what's
/// missing. Returns the assets directory. Blocking (network) — run off-thread.
pub fn ensure() -> Result<PathBuf, String> {
    let dir = dir().ok_or("no data directory")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for asset in ASSETS {
        let path = dir.join(asset.file);
        if path.exists() && digest_matches(&path, asset.sha256) {
            continue;
        }
        download(asset, &path)?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_is_https_with_a_sha256() {
        for a in ASSETS {
            assert!(a.url.starts_with("https://"), "{} must be HTTPS", a.file);
            assert_eq!(a.sha256.len(), 64, "{} needs a full sha256", a.file);
            assert!(a.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }
}
