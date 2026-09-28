//! Built-in registry of the LSP servers clew manages itself.
//!
//! One table, [`SERVERS`], holds everything clew knows about a server: its
//! pinned version, the languages it serves, how it is launched, and how it is
//! obtained — per platform, a download URL together with the SHA-256 the
//! bytes must match, or the toolchain command that builds it. Adding a server
//! means adding one row; the tests iterate the table, so a row that breaks an
//! invariant (a URL naming another version, a platform listed twice, a
//! malformed digest) fails the build or the suite rather than an install.

use std::fmt;

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

    /// Every platform, for tests that walk the whole table.
    pub const ALL: [Platform; 6] = [
        Platform::MacArm64,
        Platform::MacX64,
        Platform::LinuxArm64,
        Platform::LinuxX64,
        Platform::WindowsX64,
        Platform::WindowsArm64,
    ];
}

/// How a downloaded artifact is unpacked to yield the server executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archive {
    /// gzip of the raw executable (rust-analyzer's `.gz` assets).
    Gzip,
    /// zip archive containing the executable at `binary` (Windows assets).
    Zip,
    /// xz-compressed tar (zls's `.tar.xz` assets).
    TarXz,
    /// gzip-compressed tar (vscode-js-debug's `.tar.gz`).
    TarGz,
}

/// A SHA-256 digest.
///
/// A type rather than a hex string so that "no digest" cannot be expressed:
/// the table spells digests with [`Sha256::from_hex`], which is evaluated at
/// compile time and fails the BUILD on anything but 64 hex digits. The old
/// representation (`&str`) needed a runtime `is_empty()` guard, and grew
/// unreachable `""` branches for versions that had no digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256([u8; 32]);

impl Sha256 {
    /// Parse 64 hex digits. For use in `const`/`static` items, where bad
    /// input is a compile error; [`Sha256::parse`] is the runtime spelling.
    pub const fn from_hex(hex: &str) -> Sha256 {
        match Sha256::parse_const(hex.as_bytes()) {
            Some(digest) => digest,
            None => panic!("a SHA-256 digest is exactly 64 hex digits"),
        }
    }

    /// Parse 64 hex digits (either case).
    pub fn parse(hex: &str) -> Option<Sha256> {
        Sha256::parse_const(hex.as_bytes())
    }

    const fn parse_const(hex: &[u8]) -> Option<Sha256> {
        const fn nibble(c: u8) -> Option<u8> {
            match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                b'A'..=b'F' => Some(c - b'A' + 10),
                _ => None,
            }
        }
        if hex.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        let mut i = 0;
        while i < 32 {
            let (Some(hi), Some(lo)) = (nibble(hex[2 * i]), nibble(hex[2 * i + 1])) else {
                return None;
            };
            out[i] = (hi << 4) | lo;
            i += 1;
        }
        Some(Sha256(out))
    }

    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Sha256 {
        use sha2::Digest;
        let mut out = [0u8; 32];
        out.copy_from_slice(&sha2::Sha256::digest(bytes));
        Sha256(out)
    }

    /// Lowercase hex.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256({})", self.to_hex())
    }
}

/// A downloadable artifact for one platform, resolved from the table.
#[derive(Debug, Clone)]
pub struct Download {
    pub url: &'static str,
    /// What the downloaded bytes must hash to before anything is unpacked.
    pub sha256: Sha256,
    pub archive: Archive,
    /// Executable path after extraction, relative to the install directory.
    pub binary: &'static str,
}

/// Install via a language toolchain the user already has, into clew's store.
#[derive(Debug, Clone)]
pub struct Install {
    /// Executable that must be on PATH to perform the install (e.g. "go").
    pub tool: &'static str,
    pub kind: Installer,
    /// Executable path relative to the install directory.
    pub binary: &'static str,
    /// The version this install was prepared for — the one the consent
    /// prompt names. `store::toolchain_install_cancellable` refuses any other.
    pub version: String,
    /// One-line description of what will run, for the consent prompt:
    /// `tool` followed by exactly the arguments the spawn passes (see
    /// [`Installer::args`]); only the store destination is added at spawn.
    pub describe: String,
}

impl Install {
    fn new(tool: &'static str, kind: Installer, binary: &'static str, version: &str) -> Install {
        let describe = kind.describe(tool, version);
        Install {
            tool,
            kind,
            binary,
            version: version.to_string(),
            describe,
        }
    }

    /// The arguments the install runs with, minus the destination.
    pub fn args(&self) -> Vec<String> {
        self.kind.args(&self.version)
    }
}

#[derive(Debug, Clone)]
pub enum Installer {
    /// `go install <module>@<version>` with GOBIN pointed at the store.
    Go { module: &'static str },
    /// `npm install --prefix <dir> <packages...>` (each pinned to version).
    Npm { packages: &'static [&'static str] },
    /// `cargo install <crate> --root <dir> --features <features>`.
    Cargo {
        crate_name: &'static str,
        features: &'static [&'static str],
    },
}

impl Installer {
    /// The version-bearing package arguments this installer will pass — the
    /// part of the command that decides WHAT gets installed.
    ///
    /// A package that already carries its own `@`-suffix is left alone: it is
    /// pinned to a line that versions independently of the language server
    /// (see the `typescript@5` entry), and appending a second `@version`
    /// produced a specifier no registry can resolve.
    ///
    /// `"latest"` means "no pin" for npm, which resolves the newest itself.
    /// Go has no such default outside a module (`go install pkg` without a
    /// version is an error there), so it is spelled `@latest` — in the prompt
    /// as well as in the command, which used to disagree on exactly this.
    pub fn packages(&self, version: &str) -> Vec<String> {
        let pin = |name: &str| -> String {
            let already_pinned = name.rfind('@').is_some_and(|i| i > 0);
            if version == "latest" || already_pinned {
                name.to_string()
            } else {
                format!("{name}@{version}")
            }
        };
        match self {
            Installer::Go { module } => vec![format!("{module}@{version}")],
            Installer::Npm { packages } => packages.iter().map(|p| pin(p)).collect(),
            // cargo pins with a `--version` flag rather than in the name.
            Installer::Cargo { crate_name, .. } => vec![(*crate_name).to_string()],
        }
    }

    /// Every argument the install runs with, in order, except the store
    /// destination (`--prefix`/`--root <dir>`, or `GOBIN`), which the store
    /// appends. Single-sourced: the consent prompt and the spawn both render
    /// from here, so the prompt cannot describe a different command than the
    /// one that runs.
    pub fn args(&self, version: &str) -> Vec<String> {
        let mut args = vec!["install".to_string()];
        args.extend(self.packages(version));
        if let Installer::Cargo { features, .. } = self {
            if version != "latest" {
                args.push("--version".into());
                args.push(version.to_string());
            }
            if !features.is_empty() {
                args.push("--features".into());
                args.push(features.join(","));
            }
        }
        args
    }

    /// One line naming exactly what the install will run, for the consent
    /// prompt.
    pub fn describe(&self, tool: &str, version: &str) -> String {
        format!("{tool} {}", self.args(version).join(" "))
    }
}

/// Longest version [`is_plain_version`] accepts.
pub const MAX_VERSION_LEN: usize = 64;

/// Longest server name [`is_plain_name`] accepts (the store's limit for one
/// path component).
pub const MAX_NAME_LEN: usize = 128;

/// Whether `version` is a plain version: ASCII letters and digits, with `.`,
/// `-`, `_` and `+`, starting with a letter or digit, at most
/// [`MAX_VERSION_LEN`] bytes. `latest`, `2026-07-13`, `v0.16.2`,
/// `1.0.0+build` and Go's pseudo-versions all are.
///
/// A version can come from the project's `lsp.toml`, and it is spliced into
/// what an install RUNS (`npm install pyright@<version>`, `go install
/// <module>@<version>`, `cargo install --version <version>`). Installers read
/// far more than versions in that position: `npm:other-package` installs a
/// different package under the expected name, `file:`, `git+https:` and
/// `github:` specs fetch code from anywhere, and a leading `-` is an option.
/// None of that survives this grammar — no `:`, `/`, `@`, spaces, range
/// operators, or control characters (which would also have rendered, raw,
/// into the consent prompt).
pub fn is_plain_version(version: &str) -> bool {
    is_plain_token(version, MAX_VERSION_LEN)
}

/// Whether `name` is a plain server name — the same grammar as
/// [`is_plain_version`], up to [`MAX_NAME_LEN`] bytes. The name picks a
/// registry row and a store directory, and is shown in the consent prompt.
pub fn is_plain_name(name: &str) -> bool {
    is_plain_token(name, MAX_NAME_LEN)
}

fn is_plain_token(s: &str, max: usize) -> bool {
    s.len() <= max
        && s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+'))
}

/// How clew obtains a server: a verified binary download, or a toolchain build.
#[derive(Debug, Clone)]
pub enum Provision {
    Download(Download),
    Install(Install),
    /// The server ships inside a language toolchain the user already has (e.g.
    /// `dart language-server`); run the named binary found on PATH directly.
    Toolchain {
        binary: &'static str,
    },
}

/// One downloadable build of a server: the platforms it serves, where it
/// lives, and the digest it must match.
#[derive(Debug, Clone, Copy)]
pub struct Artifact {
    pub platforms: &'static [Platform],
    /// The full URL, pinned version included — nothing is formatted in at
    /// runtime, so a digest can only ever be checked against the file it was
    /// taken from.
    pub url: &'static str,
    pub sha256: Sha256,
    pub archive: Archive,
    pub binary: &'static str,
}

/// Where a server comes from.
#[derive(Debug, Clone)]
pub enum Source {
    /// A verified download, available ONLY at [`ServerSpec::version`]: the
    /// digests are for that release alone.
    Download(&'static [Artifact]),
    /// A toolchain install at whatever version the project asks for.
    Install {
        tool: &'static str,
        kind: Installer,
        binary: &'static str,
    },
    /// Bundled in a toolchain found on PATH.
    Toolchain { binary: &'static str },
}

/// A version-pinned server and the languages it serves.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub name: &'static str,
    pub version: &'static str,
    pub languages: &'static [&'static str],
    /// Extra CLI args to launch the server (LSP over stdio by default).
    pub args: &'static [&'static str],
    pub source: Source,
}

impl ServerSpec {
    /// How to obtain this server at `version` on `platform`, if clew supports
    /// it there.
    ///
    /// `version` is the EFFECTIVE version — the project's `lsp.toml` override
    /// when it set one, otherwise this spec's pin. It must be threaded in
    /// rather than read from `self`: the install directory and the spawned
    /// command already used the effective version while the consent prompt
    /// used the pin, so the two could name different versions.
    ///
    /// A download provision is available ONLY at the pinned version: its
    /// SHA-256 digests are compiled in for that release alone, so any other
    /// version has nothing to verify against and is refused here rather than
    /// downloaded unverified.
    pub fn provision(&self, version: &str, platform: Platform) -> Option<Provision> {
        match &self.source {
            Source::Download(artifacts) => {
                if version != self.version {
                    return None;
                }
                let artifact = artifacts.iter().find(|a| a.platforms.contains(&platform))?;
                Some(Provision::Download(Download {
                    url: artifact.url,
                    sha256: artifact.sha256,
                    archive: artifact.archive,
                    binary: artifact.binary,
                }))
            }
            // The version goes into the install's argv: nothing but a plain
            // one may get that far, whoever the caller is.
            Source::Install { .. } if !is_plain_version(version) => None,
            Source::Install { tool, kind, binary } => Some(Provision::Install(Install::new(
                tool,
                kind.clone(),
                binary,
                version,
            ))),
            Source::Toolchain { binary } => Some(Provision::Toolchain { binary }),
        }
    }
}

/// Every server clew knows how to provision. The first server listing a
/// language is that language's default.
pub static SERVERS: &[ServerSpec] = &[
    ServerSpec {
        name: "rust-analyzer",
        version: "2026-07-13",
        languages: &["rust"],
        args: &[],
        // Sourced from the GitHub release asset digests for the pinned tag.
        source: Source::Download(&[
            Artifact {
                platforms: &[Platform::MacArm64],
                url: "https://github.com/rust-lang/rust-analyzer/releases/download/2026-07-13/rust-analyzer-aarch64-apple-darwin.gz",
                sha256: Sha256::from_hex(
                    "9c6b3ebf06480e2c95a7b01750fa68d77834bffa34da81e4eb00cef3cdff4613",
                ),
                archive: Archive::Gzip,
                binary: "rust-analyzer",
            },
            Artifact {
                platforms: &[Platform::MacX64],
                url: "https://github.com/rust-lang/rust-analyzer/releases/download/2026-07-13/rust-analyzer-x86_64-apple-darwin.gz",
                sha256: Sha256::from_hex(
                    "b8832accb9f163214e63ccc989bb2161d52f19270eafb136da0fb16093185041",
                ),
                archive: Archive::Gzip,
                binary: "rust-analyzer",
            },
            Artifact {
                platforms: &[Platform::LinuxArm64],
                url: "https://github.com/rust-lang/rust-analyzer/releases/download/2026-07-13/rust-analyzer-aarch64-unknown-linux-gnu.gz",
                sha256: Sha256::from_hex(
                    "d30c3ac726f93ae7cb57c6e16cd2d2b5460c9893ccdd38b6d3ae9300c72852ab",
                ),
                archive: Archive::Gzip,
                binary: "rust-analyzer",
            },
            Artifact {
                platforms: &[Platform::LinuxX64],
                url: "https://github.com/rust-lang/rust-analyzer/releases/download/2026-07-13/rust-analyzer-x86_64-unknown-linux-gnu.gz",
                sha256: Sha256::from_hex(
                    "5ee1754afa7a1eb7f56606847b61328e6fac2f316e40ebf314dcefb30263df4d",
                ),
                archive: Archive::Gzip,
                binary: "rust-analyzer",
            },
            Artifact {
                platforms: &[Platform::WindowsX64],
                url: "https://github.com/rust-lang/rust-analyzer/releases/download/2026-07-13/rust-analyzer-x86_64-pc-windows-msvc.zip",
                sha256: Sha256::from_hex(
                    "d8d79975da21ca59dcb9d19d253b2210f33c3eef41eab24ec48b6d5aaaa4e1ff",
                ),
                archive: Archive::Zip,
                binary: "rust-analyzer.exe",
            },
            Artifact {
                platforms: &[Platform::WindowsArm64],
                url: "https://github.com/rust-lang/rust-analyzer/releases/download/2026-07-13/rust-analyzer-aarch64-pc-windows-msvc.zip",
                sha256: Sha256::from_hex(
                    "ae7bf858503bb26f49d5ef9bd5d14a61f78573ea706225cdbc39cb52afe9f9f7",
                ),
                archive: Archive::Zip,
                binary: "rust-analyzer.exe",
            },
        ]),
    },
    ServerSpec {
        name: "clangd",
        version: "22.1.6",
        languages: &["c", "cpp"],
        args: &[],
        // One build per OS (the macOS one is universal). It needs its bundled
        // `lib/` resource tree, so the whole zip is extracted. No linux-arm64
        // build is published.
        source: Source::Download(&[
            Artifact {
                platforms: &[Platform::MacArm64, Platform::MacX64],
                url: "https://github.com/clangd/clangd/releases/download/22.1.6/clangd-mac-22.1.6.zip",
                sha256: Sha256::from_hex(
                    "631aef462556cbd74e0ebaae1778a38d1997d0ba3371652ca54f82652a179e7d",
                ),
                archive: Archive::Zip,
                binary: "clangd_22.1.6/bin/clangd",
            },
            Artifact {
                platforms: &[Platform::LinuxX64],
                url: "https://github.com/clangd/clangd/releases/download/22.1.6/clangd-linux-22.1.6.zip",
                sha256: Sha256::from_hex(
                    "a9c77443af2e447ed467e84771848d3a6ac1c56f84bcfcde717e66318de77cfa",
                ),
                archive: Archive::Zip,
                binary: "clangd_22.1.6/bin/clangd",
            },
            Artifact {
                platforms: &[Platform::WindowsX64, Platform::WindowsArm64],
                url: "https://github.com/clangd/clangd/releases/download/22.1.6/clangd-windows-22.1.6.zip",
                sha256: Sha256::from_hex(
                    "ce54f16e0b4fd76d450eeda9664420b195360b73febcfe40e661108fa57f2ce1",
                ),
                archive: Archive::Zip,
                binary: "clangd_22.1.6/bin/clangd.exe",
            },
        ]),
    },
    ServerSpec {
        name: "gopls",
        version: "latest",
        languages: &["go"],
        args: &[],
        source: Source::Install {
            tool: "go",
            kind: Installer::Go {
                module: "golang.org/x/tools/gopls",
            },
            binary: "gopls",
        },
    },
    ServerSpec {
        name: "dart",
        version: "sdk",
        languages: &["dart"],
        args: &["language-server", "--protocol=lsp"],
        // Dart's LSP is `dart language-server`, bundled with the Dart/Flutter
        // SDK — run the toolchain binary directly rather than installing one.
        source: Source::Toolchain { binary: "dart" },
    },
    ServerSpec {
        name: "pyright",
        version: "latest",
        languages: &["python"],
        args: &["--stdio"],
        source: Source::Install {
            tool: "npm",
            kind: Installer::Npm {
                packages: &["pyright"],
            },
            binary: "node_modules/.bin/pyright-langserver",
        },
    },
    ServerSpec {
        name: "typescript-language-server",
        version: "latest",
        languages: &["typescript", "tsx", "javascript"],
        args: &["--stdio"],
        source: Source::Install {
            tool: "npm",
            // `typescript` is pinned to the 5.x line: as of TS 7 the default
            // `typescript` dist-tag is the native Go port (tsgo), which ships
            // no classic `lib/tsserver.js` — the tsserver protocol that
            // typescript-language-server drives. Unpinned, `npm install
            // typescript` pulls 7.x and the server aborts with "Could not
            // find a valid TypeScript installation." The per-package pin is
            // embedded here because the shared version is "latest" for the
            // language server, which versions independently of TS.
            kind: Installer::Npm {
                packages: &["typescript-language-server", "typescript@5"],
            },
            binary: "node_modules/.bin/typescript-language-server",
        },
    },
    // json / html / css all come from one npm package, launched via their own
    // binaries.
    ServerSpec {
        name: "vscode-json-language-server",
        version: "latest",
        languages: &["json"],
        args: &["--stdio"],
        source: Source::Install {
            tool: "npm",
            kind: Installer::Npm {
                packages: &["vscode-langservers-extracted"],
            },
            binary: "node_modules/.bin/vscode-json-language-server",
        },
    },
    ServerSpec {
        name: "vscode-html-language-server",
        version: "latest",
        languages: &["html"],
        args: &["--stdio"],
        source: Source::Install {
            tool: "npm",
            kind: Installer::Npm {
                packages: &["vscode-langservers-extracted"],
            },
            binary: "node_modules/.bin/vscode-html-language-server",
        },
    },
    ServerSpec {
        name: "vscode-css-language-server",
        version: "latest",
        languages: &["css"],
        args: &["--stdio"],
        source: Source::Install {
            tool: "npm",
            kind: Installer::Npm {
                packages: &["vscode-langservers-extracted"],
            },
            binary: "node_modules/.bin/vscode-css-language-server",
        },
    },
    ServerSpec {
        name: "taplo",
        version: "latest",
        languages: &["toml"],
        args: &["lsp", "stdio"],
        // The npm @taplo/cli build has no language server; the native binary
        // built with the `lsp` feature does.
        source: Source::Install {
            tool: "cargo",
            kind: Installer::Cargo {
                crate_name: "taplo-cli",
                features: &["lsp"],
            },
            binary: "bin/taplo",
        },
    },
    ServerSpec {
        name: "zls",
        version: "0.16.0",
        languages: &["zig"],
        args: &[],
        // One `.tar.xz` per mac/linux target. zls publishes Windows as zip;
        // no pinned digest is wired for it, so Windows has no zls.
        source: Source::Download(&[
            Artifact {
                platforms: &[Platform::MacArm64],
                url: "https://github.com/zigtools/zls/releases/download/0.16.0/zls-aarch64-macos.tar.xz",
                sha256: Sha256::from_hex(
                    "b93ec549f8558a7e85984a840e9276d274f1059b54ade4254296ef4982958359",
                ),
                archive: Archive::TarXz,
                binary: "zls",
            },
            Artifact {
                platforms: &[Platform::MacX64],
                url: "https://github.com/zigtools/zls/releases/download/0.16.0/zls-x86_64-macos.tar.xz",
                sha256: Sha256::from_hex(
                    "49f716ea96c1aadaecaa5d9c0a50874cbcf443dc42b825f1e7ee35499ad3eb96",
                ),
                archive: Archive::TarXz,
                binary: "zls",
            },
            Artifact {
                platforms: &[Platform::LinuxArm64],
                url: "https://github.com/zigtools/zls/releases/download/0.16.0/zls-aarch64-linux.tar.xz",
                sha256: Sha256::from_hex(
                    "430cd293d201eb70ae2519dbc96c854bf8791b8df7fc9392e8d2dc9680a2bed7",
                ),
                archive: Archive::TarXz,
                binary: "zls",
            },
            Artifact {
                platforms: &[Platform::LinuxX64],
                url: "https://github.com/zigtools/zls/releases/download/0.16.0/zls-x86_64-linux.tar.xz",
                sha256: Sha256::from_hex(
                    "ded6d562a0b86ee878b1ddf70ffab2797ce3cdca3b02d6077548f9d56dff96b6",
                ),
                archive: Archive::TarXz,
                binary: "zls",
            },
        ]),
    },
];

/// All servers clew knows how to provision.
pub fn all() -> &'static [ServerSpec] {
    SERVERS
}

/// The default server for a language, if clew ships one.
pub fn default_for_language(language: &str) -> Option<&'static ServerSpec> {
    SERVERS.iter().find(|s| s.languages.contains(&language))
}

/// Look up a server by name.
pub fn by_name(name: &str) -> Option<&'static ServerSpec> {
    SERVERS.iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn download(spec: &ServerSpec, platform: Platform) -> Option<Download> {
        match spec.provision(spec.version, platform)? {
            Provision::Download(d) => Some(d),
            Provision::Install(_) | Provision::Toolchain { .. } => None,
        }
    }

    /// The table must reproduce, byte for byte, the artifacts the previous
    /// hand-written resolver produced (captured from it before the table
    /// replaced it): a typo in a URL or a digest here is a broken or, worse,
    /// a silently different install.
    #[test]
    fn the_table_reproduces_the_pinned_artifacts() {
        let expected: &[(&str, Platform, &str, &str, Archive, &str)] = &[
            (
                "rust-analyzer",
                Platform::MacArm64,
                "https://github.com/rust-lang/rust-analyzer/releases/download/2026-07-13/rust-analyzer-aarch64-apple-darwin.gz",
                "9c6b3ebf06480e2c95a7b01750fa68d77834bffa34da81e4eb00cef3cdff4613",
                Archive::Gzip,
                "rust-analyzer",
            ),
            (
                "rust-analyzer",
                Platform::MacX64,
                "https://github.com/rust-lang/rust-analyzer/releases/download/2026-07-13/rust-analyzer-x86_64-apple-darwin.gz",
                "b8832accb9f163214e63ccc989bb2161d52f19270eafb136da0fb16093185041",
                Archive::Gzip,
                "rust-analyzer",
            ),
            (
                "rust-analyzer",
                Platform::LinuxArm64,
                "https://github.com/rust-lang/rust-analyzer/releases/download/2026-07-13/rust-analyzer-aarch64-unknown-linux-gnu.gz",
                "d30c3ac726f93ae7cb57c6e16cd2d2b5460c9893ccdd38b6d3ae9300c72852ab",
                Archive::Gzip,
                "rust-analyzer",
            ),
            (
                "rust-analyzer",
                Platform::LinuxX64,
                "https://github.com/rust-lang/rust-analyzer/releases/download/2026-07-13/rust-analyzer-x86_64-unknown-linux-gnu.gz",
                "5ee1754afa7a1eb7f56606847b61328e6fac2f316e40ebf314dcefb30263df4d",
                Archive::Gzip,
                "rust-analyzer",
            ),
            (
                "rust-analyzer",
                Platform::WindowsX64,
                "https://github.com/rust-lang/rust-analyzer/releases/download/2026-07-13/rust-analyzer-x86_64-pc-windows-msvc.zip",
                "d8d79975da21ca59dcb9d19d253b2210f33c3eef41eab24ec48b6d5aaaa4e1ff",
                Archive::Zip,
                "rust-analyzer.exe",
            ),
            (
                "rust-analyzer",
                Platform::WindowsArm64,
                "https://github.com/rust-lang/rust-analyzer/releases/download/2026-07-13/rust-analyzer-aarch64-pc-windows-msvc.zip",
                "ae7bf858503bb26f49d5ef9bd5d14a61f78573ea706225cdbc39cb52afe9f9f7",
                Archive::Zip,
                "rust-analyzer.exe",
            ),
            (
                "clangd",
                Platform::MacArm64,
                "https://github.com/clangd/clangd/releases/download/22.1.6/clangd-mac-22.1.6.zip",
                "631aef462556cbd74e0ebaae1778a38d1997d0ba3371652ca54f82652a179e7d",
                Archive::Zip,
                "clangd_22.1.6/bin/clangd",
            ),
            (
                "clangd",
                Platform::MacX64,
                "https://github.com/clangd/clangd/releases/download/22.1.6/clangd-mac-22.1.6.zip",
                "631aef462556cbd74e0ebaae1778a38d1997d0ba3371652ca54f82652a179e7d",
                Archive::Zip,
                "clangd_22.1.6/bin/clangd",
            ),
            (
                "clangd",
                Platform::LinuxX64,
                "https://github.com/clangd/clangd/releases/download/22.1.6/clangd-linux-22.1.6.zip",
                "a9c77443af2e447ed467e84771848d3a6ac1c56f84bcfcde717e66318de77cfa",
                Archive::Zip,
                "clangd_22.1.6/bin/clangd",
            ),
            (
                "clangd",
                Platform::WindowsX64,
                "https://github.com/clangd/clangd/releases/download/22.1.6/clangd-windows-22.1.6.zip",
                "ce54f16e0b4fd76d450eeda9664420b195360b73febcfe40e661108fa57f2ce1",
                Archive::Zip,
                "clangd_22.1.6/bin/clangd.exe",
            ),
            (
                "clangd",
                Platform::WindowsArm64,
                "https://github.com/clangd/clangd/releases/download/22.1.6/clangd-windows-22.1.6.zip",
                "ce54f16e0b4fd76d450eeda9664420b195360b73febcfe40e661108fa57f2ce1",
                Archive::Zip,
                "clangd_22.1.6/bin/clangd.exe",
            ),
            (
                "zls",
                Platform::MacArm64,
                "https://github.com/zigtools/zls/releases/download/0.16.0/zls-aarch64-macos.tar.xz",
                "b93ec549f8558a7e85984a840e9276d274f1059b54ade4254296ef4982958359",
                Archive::TarXz,
                "zls",
            ),
            (
                "zls",
                Platform::MacX64,
                "https://github.com/zigtools/zls/releases/download/0.16.0/zls-x86_64-macos.tar.xz",
                "49f716ea96c1aadaecaa5d9c0a50874cbcf443dc42b825f1e7ee35499ad3eb96",
                Archive::TarXz,
                "zls",
            ),
            (
                "zls",
                Platform::LinuxArm64,
                "https://github.com/zigtools/zls/releases/download/0.16.0/zls-aarch64-linux.tar.xz",
                "430cd293d201eb70ae2519dbc96c854bf8791b8df7fc9392e8d2dc9680a2bed7",
                Archive::TarXz,
                "zls",
            ),
            (
                "zls",
                Platform::LinuxX64,
                "https://github.com/zigtools/zls/releases/download/0.16.0/zls-x86_64-linux.tar.xz",
                "ded6d562a0b86ee878b1ddf70ffab2797ce3cdca3b02d6077548f9d56dff96b6",
                Archive::TarXz,
                "zls",
            ),
        ];
        let mut seen = 0;
        for spec in all() {
            for platform in Platform::ALL {
                let Some(dl) = download(spec, platform) else {
                    continue;
                };
                seen += 1;
                let want = expected
                    .iter()
                    .find(|e| e.0 == spec.name && e.1 == platform)
                    .unwrap_or_else(|| panic!("unexpected artifact {} {platform:?}", spec.name));
                assert_eq!(dl.url, want.2);
                assert_eq!(dl.sha256.to_hex(), want.3);
                assert_eq!(dl.archive, want.4);
                assert_eq!(dl.binary, want.5);
            }
        }
        assert_eq!(seen, expected.len(), "an artifact went missing");
        // The platforms with no build stay without one.
        let clangd = by_name("clangd").unwrap();
        assert!(
            clangd
                .provision(clangd.version, Platform::LinuxArm64)
                .is_none()
        );
        let zls = by_name("zls").unwrap();
        assert!(zls.provision(zls.version, Platform::WindowsX64).is_none());
    }

    /// Invariants every row must keep, checked over the whole table so a new
    /// server cannot slip past them.
    #[test]
    fn every_row_is_well_formed() {
        let mut names = std::collections::HashSet::new();
        for spec in all() {
            assert!(names.insert(spec.name), "{} listed twice", spec.name);
            assert!(!spec.languages.is_empty(), "{} serves nothing", spec.name);
            match &spec.source {
                Source::Download(artifacts) => {
                    let mut platforms = Vec::new();
                    for a in *artifacts {
                        assert!(a.url.starts_with("https://"), "{}", a.url);
                        // The digest belongs to one release: the URL must name
                        // the pinned version, or they describe different files.
                        assert!(
                            a.url.contains(&format!("/{}/", spec.version)),
                            "{} does not name version {}",
                            a.url,
                            spec.version
                        );
                        assert!(
                            !a.binary.is_empty()
                                && !a.binary.starts_with('/')
                                && !a.binary.split('/').any(|c| c == ".."),
                            "binary must be a plain relative path: {}",
                            a.binary
                        );
                        for p in a.platforms {
                            assert!(
                                !platforms.contains(p),
                                "{} has two artifacts for {p:?}",
                                spec.name
                            );
                            platforms.push(*p);
                        }
                    }
                }
                Source::Install { tool, binary, .. } => {
                    assert!(!tool.contains('/'), "a tool is looked up on PATH");
                    assert!(!binary.starts_with('/') && !binary.contains(".."));
                }
                Source::Toolchain { binary } => assert!(!binary.contains('/')),
            }
        }
    }

    #[test]
    fn digests_parse_strictly() {
        let hex = "9c6b3ebf06480e2c95a7b01750fa68d77834bffa34da81e4eb00cef3cdff4613";
        let d = Sha256::parse(hex).unwrap();
        assert_eq!(d.to_hex(), hex);
        assert_eq!(Sha256::parse(&hex.to_uppercase()), Some(d));
        assert_eq!(Sha256::from_hex(hex), d);
        for bad in [
            "",
            &hex[1..],
            &format!("{hex}0"),
            &format!("g{}", &hex[1..]),
        ] {
            assert_eq!(Sha256::parse(bad), None, "{bad:?}");
        }
        assert_eq!(
            Sha256::of(b"abc").to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn rust_is_registered_with_pinned_download() {
        let spec = default_for_language("rust").expect("rust server");
        assert_eq!(spec.name, "rust-analyzer");
        let dl = download(spec, Platform::MacArm64).expect("mac arm download");
        assert!(dl.url.contains(spec.version));
        assert!(dl.url.ends_with("aarch64-apple-darwin.gz"));
        assert_eq!(dl.archive, Archive::Gzip);
        assert_eq!(dl.binary, "rust-analyzer");
    }

    #[test]
    fn go_python_ts_use_toolchain_install() {
        let go = default_for_language("go").expect("go server");
        assert_eq!(go.name, "gopls");
        match go.provision(go.version, Platform::MacArm64).unwrap() {
            Provision::Install(i) => {
                assert_eq!(i.tool, "go");
                assert_eq!(i.binary, "gopls");
                assert!(matches!(i.kind, Installer::Go { .. }));
            }
            _ => panic!("gopls should be a toolchain install"),
        }
        let py = default_for_language("python").unwrap();
        match py.provision(py.version, Platform::MacArm64).unwrap() {
            Provision::Install(i) => {
                assert_eq!(i.tool, "npm");
                assert!(i.binary.ends_with("pyright-langserver"));
            }
            _ => panic!("pyright should be a toolchain install"),
        }
        assert_eq!(
            default_for_language("typescript").unwrap().name,
            "typescript-language-server"
        );
        assert_eq!(default_for_language("c").unwrap().name, "clangd");
        assert_eq!(default_for_language("zig").unwrap().name, "zls");
        assert_eq!(
            default_for_language("css").unwrap().name,
            "vscode-css-language-server"
        );
        assert!(matches!(
            default_for_language("dart")
                .unwrap()
                .provision("sdk", Platform::MacArm64),
            Some(Provision::Toolchain { binary: "dart" })
        ));
    }

    #[test]
    fn toml_uses_cargo_for_the_lsp_enabled_binary() {
        // The npm @taplo/cli has no LSP; the native cargo build does.
        let spec = default_for_language("toml").expect("toml server");
        assert_eq!(spec.name, "taplo");
        assert_eq!(spec.args, &["lsp", "stdio"]);
        match spec.provision(spec.version, Platform::MacArm64).unwrap() {
            Provision::Install(i) => {
                assert_eq!(i.tool, "cargo");
                assert_eq!(i.binary, "bin/taplo");
                assert_eq!(i.describe, "cargo install taplo-cli --features lsp");
            }
            _ => panic!("toml should be a cargo install"),
        }
    }

    #[test]
    fn config_languages_use_one_npm_package() {
        for (lang, server) in [
            ("json", "vscode-json-language-server"),
            ("html", "vscode-html-language-server"),
            ("css", "vscode-css-language-server"),
        ] {
            let spec = default_for_language(lang).unwrap_or_else(|| panic!("no server for {lang}"));
            assert_eq!(spec.name, server);
            match spec.provision(spec.version, Platform::MacArm64).unwrap() {
                Provision::Install(i) => {
                    assert_eq!(i.tool, "npm");
                    assert_eq!(i.args(), ["install", "vscode-langservers-extracted"]);
                    assert!(i.binary.ends_with(server), "{}", i.binary);
                }
                _ => panic!("{lang} should be an npm install"),
            }
        }
    }

    #[test]
    fn unknown_language_has_no_server() {
        assert!(default_for_language("cobol").is_none());
        assert!(by_name("cobol-ls").is_none());
    }
}

#[cfg(test)]
mod version_consistency_tests {
    use super::*;

    /// F2: `version` from a repository's `lsp.toml` lands in the install
    /// command, where installers read much more than versions.
    #[test]
    fn only_plain_versions_are_accepted() {
        for good in [
            "latest",
            "sdk",
            "next",
            "2026-07-13",
            "0.16.0",
            "v0.99.0",
            "1.0.0+build.5",
            "v0.0.0-20230101000000-abcdef123456",
            "1.2.3_rc1",
        ] {
            assert!(is_plain_version(good), "{good:?}");
        }
        for bad in [
            "",
            "npm:evil-pkg",
            "file:../payload",
            "git+https://example.invalid/x.git",
            "github:user/repo",
            "user/repo",
            "@scope/pkg",
            "1.2.3@x",
            "^1.2",
            "~1.2",
            ">=1.0.0",
            "1 || 2",
            "1.2.3 ",
            "-rf",
            "--registry=https://evil.invalid",
            ".hidden",
            "x\u{1b}[31mred",
            "1.2.3\n",
            "é1",
            &"9".repeat(MAX_VERSION_LEN + 1),
        ] {
            assert!(!is_plain_version(bad), "{bad:?}");
        }
        assert!(is_plain_version(&"9".repeat(MAX_VERSION_LEN)));
        assert!(is_plain_name("vscode-json-language-server"));
        assert!(!is_plain_name("evil\u{7}"));
        for spec in all() {
            assert!(is_plain_name(spec.name), "{}", spec.name);
            assert!(is_plain_version(spec.version), "{}", spec.version);
        }
    }

    /// …and the registry itself refuses to build an install from anything
    /// else, so no caller can get `npm install pyright@npm:evil-pkg` out of it.
    #[test]
    fn an_install_is_never_built_from_a_version_that_is_not_plain() {
        for spec in all() {
            if matches!(spec.source, Source::Install { .. }) {
                assert!(
                    spec.provision("npm:evil-pkg", Platform::MacArm64).is_none(),
                    "{}",
                    spec.name
                );
            }
        }
    }

    fn install(name: &str, version: &str) -> Install {
        match by_name(name)
            .unwrap()
            .provision(version, Platform::MacArm64)
        {
            Some(Provision::Install(i)) => i,
            other => panic!("{name} installs via a toolchain, got {other:?}"),
        }
    }

    /// The consent prompt and the spawned command must name the same version.
    /// They were built independently — the prompt from the registry pin, the
    /// command from the project's `lsp.toml` override — so a repository could
    /// have the user approve one version and clew install another.
    #[test]
    fn the_consent_prompt_names_the_version_that_will_be_installed() {
        let pinned = install("gopls", "v0.99.0");
        assert_eq!(pinned.version, "v0.99.0");
        assert_eq!(
            pinned.describe,
            "go install golang.org/x/tools/gopls@v0.99.0"
        );
        // And the prompt is rendered from the very list the spawn passes.
        assert_eq!(pinned.describe, format!("go {}", pinned.args().join(" ")));
    }

    /// Go has no implicit "latest" outside a module, so the command always
    /// ran `gopls@latest` — while the prompt said plain `gopls`. Both now say
    /// what runs.
    #[test]
    fn latest_is_spelled_out_for_go_and_left_to_npm() {
        let gopls = install("gopls", "latest");
        assert_eq!(gopls.args(), ["install", "golang.org/x/tools/gopls@latest"]);
        assert_eq!(gopls.describe, "go install golang.org/x/tools/gopls@latest");
        let pyright = install("pyright", "latest");
        assert_eq!(pyright.args(), ["install", "pyright"]);
        assert_eq!(pyright.describe, "npm install pyright");
    }

    /// A package carrying its own `@`-pin versions independently of the
    /// language server; appending a second suffix produced `typescript@5@1.2.3`,
    /// which no registry can resolve.
    #[test]
    fn a_package_with_its_own_pin_is_not_pinned_twice() {
        let ts = install("typescript-language-server", "1.2.3");
        let args = ts.args();
        assert!(
            args.contains(&"typescript@5".to_string()),
            "the independently-pinned package must be left alone, got {args:?}"
        );
        assert!(
            args.contains(&"typescript-language-server@1.2.3".to_string()),
            "the server itself still takes the requested version, got {args:?}"
        );
        assert!(!args.iter().any(|a| a.matches('@').count() > 1), "{args:?}");
    }

    #[test]
    fn cargo_pins_with_a_flag() {
        let taplo = install("taplo", "0.9.3");
        assert_eq!(
            taplo.args(),
            [
                "install",
                "taplo-cli",
                "--version",
                "0.9.3",
                "--features",
                "lsp"
            ]
        );
        assert_eq!(
            taplo.describe,
            "cargo install taplo-cli --version 0.9.3 --features lsp"
        );
    }

    /// A downloaded server is verified against a digest compiled in for ONE
    /// release. Asking for any other version must resolve to nothing, not to a
    /// download that cannot be verified.
    #[test]
    fn a_download_is_offered_only_at_the_version_its_digest_covers() {
        for spec in all() {
            if !matches!(spec.source, Source::Download(_)) {
                continue;
            }
            assert!(
                Platform::ALL
                    .iter()
                    .any(|p| spec.provision(spec.version, *p).is_some()),
                "{} provisions at its pin",
                spec.name
            );
            for p in Platform::ALL {
                assert!(
                    spec.provision("2099-01-01", p).is_none(),
                    "{} at an unpinned version has no digest and must be refused",
                    spec.name
                );
            }
        }
    }
}
