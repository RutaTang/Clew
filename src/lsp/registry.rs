//! Built-in registry of the LSP servers clew manages itself.
//!
//! clew ships its own version-pinned servers and never uses the system PATH.
//! Each entry pins a version and, per platform, a download URL + SHA-256 so a
//! download can be verified before the binary is ever executed.

/// Host platform, as the subset of targets we publish servers for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacArm64,
    MacX64,
    LinuxArm64,
    LinuxX64,
    WindowsX64,
    WindowsArm64,
}

impl Platform {
    /// The platform clew is running on, or `None` if unsupported.
    pub fn current() -> Option<Self> {
        Some(match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => Self::MacArm64,
            ("macos", "x86_64") => Self::MacX64,
            ("linux", "aarch64") => Self::LinuxArm64,
            ("linux", "x86_64") => Self::LinuxX64,
            ("windows", "x86_64") => Self::WindowsX64,
            ("windows", "aarch64") => Self::WindowsArm64,
            _ => return None,
        })
    }
}

/// How a downloaded artifact is unpacked to yield the server executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archive {
    /// gzip of the raw executable (rust-analyzer's `.gz` assets).
    Gzip,
    /// zip archive containing the executable at `binary` (Windows assets).
    Zip,
}

/// A downloadable artifact for one platform.
#[derive(Debug, Clone)]
pub struct Download {
    pub url: String,
    pub sha256: &'static str,
    pub archive: Archive,
    /// Executable file name after extraction (also the installed name).
    pub binary: &'static str,
}

/// A version-pinned server and the languages it serves.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub name: &'static str,
    pub version: &'static str,
    pub languages: &'static [&'static str],
    /// Extra CLI args to launch the server (LSP over stdio by default).
    pub args: &'static [&'static str],
}

impl ServerSpec {
    /// Resolve the download for `platform`, if this server ships for it.
    pub fn download(&self, platform: Platform) -> Option<Download> {
        (self.name == "rust-analyzer").then(|| rust_analyzer_download(self.version, platform))?
    }
}

/// All servers clew knows how to provision.
pub fn all() -> Vec<ServerSpec> {
    vec![ServerSpec {
        name: "rust-analyzer",
        version: RUST_ANALYZER_VERSION,
        languages: &["rust"],
        args: &[],
    }]
}

/// The default server for a language, if clew ships one.
pub fn default_for_language(language: &str) -> Option<ServerSpec> {
    all().into_iter().find(|s| s.languages.contains(&language))
}

/// Look up a server by name.
pub fn by_name(name: &str) -> Option<ServerSpec> {
    all().into_iter().find(|s| s.name == name)
}

// --------------------------------------------------------------- rust-analyzer

const RUST_ANALYZER_VERSION: &str = "2026-07-13";

/// SHA-256 digests for the pinned rust-analyzer release, per platform.
/// Sourced from the GitHub release asset digests for the pinned tag.
fn rust_analyzer_download(version: &str, platform: Platform) -> Option<Download> {
    // (target triple, extension, sha256 for RUST_ANALYZER_VERSION)
    let (triple, ext, sha256) = match platform {
        Platform::MacArm64 => (
            "aarch64-apple-darwin",
            "gz",
            "9c6b3ebf06480e2c95a7b01750fa68d77834bffa34da81e4eb00cef3cdff4613",
        ),
        Platform::MacX64 => (
            "x86_64-apple-darwin",
            "gz",
            "b8832accb9f163214e63ccc989bb2161d52f19270eafb136da0fb16093185041",
        ),
        Platform::LinuxArm64 => (
            "aarch64-unknown-linux-gnu",
            "gz",
            "d30c3ac726f93ae7cb57c6e16cd2d2b5460c9893ccdd38b6d3ae9300c72852ab",
        ),
        Platform::LinuxX64 => (
            "x86_64-unknown-linux-gnu",
            "gz",
            "5ee1754afa7a1eb7f56606847b61328e6fac2f316e40ebf314dcefb30263df4d",
        ),
        Platform::WindowsX64 => (
            "x86_64-pc-windows-msvc",
            "zip",
            "d8d79975da21ca59dcb9d19d253b2210f33c3eef41eab24ec48b6d5aaaa4e1ff",
        ),
        Platform::WindowsArm64 => (
            "aarch64-pc-windows-msvc",
            "zip",
            "ae7bf858503bb26f49d5ef9bd5d14a61f78573ea706225cdbc39cb52afe9f9f7",
        ),
    };
    // Checksums are only valid for the pinned version; other versions must be
    // verified against a digest fetched at download time.
    let sha256 = if version == RUST_ANALYZER_VERSION {
        sha256
    } else {
        ""
    };
    let (archive, binary) = match ext {
        "zip" => (Archive::Zip, "rust-analyzer.exe"),
        _ => (Archive::Gzip, "rust-analyzer"),
    };
    Some(Download {
        url: format!(
            "https://github.com/rust-lang/rust-analyzer/releases/download/{version}/rust-analyzer-{triple}.{ext}"
        ),
        sha256,
        archive,
        binary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_is_registered_with_pinned_download() {
        let spec = default_for_language("rust").expect("rust server");
        assert_eq!(spec.name, "rust-analyzer");
        assert_eq!(spec.version, RUST_ANALYZER_VERSION);

        let dl = spec.download(Platform::MacArm64).expect("mac arm download");
        assert!(dl.url.contains(RUST_ANALYZER_VERSION));
        assert!(dl.url.ends_with("aarch64-apple-darwin.gz"));
        assert_eq!(dl.sha256.len(), 64);
        assert_eq!(dl.archive, Archive::Gzip);
        assert_eq!(dl.binary, "rust-analyzer");
    }

    #[test]
    fn windows_uses_zip_and_exe() {
        let spec = default_for_language("rust").unwrap();
        let dl = spec.download(Platform::WindowsX64).unwrap();
        assert_eq!(dl.archive, Archive::Zip);
        assert_eq!(dl.binary, "rust-analyzer.exe");
    }

    #[test]
    fn unknown_language_has_no_server() {
        assert!(default_for_language("cobol").is_none());
    }

    #[test]
    fn overridden_version_builds_url_but_drops_baked_checksum() {
        let spec = ServerSpec {
            name: "rust-analyzer",
            version: "2099-01-01",
            languages: &["rust"],
            args: &[],
        };
        let dl = spec.download(Platform::MacArm64).unwrap();
        assert!(dl.url.contains("2099-01-01"));
        assert_eq!(dl.sha256, ""); // must be verified against a fetched digest
    }
}
