//! Building a plugin from its source: `plugin.toml` and `src/**/*.rs`.
//!
//! Building runs the Rust compiler on code an agent wrote, so the build is
//! the part of plugins that runs anything natively, and it is held to that:
//!
//! - Clew writes `Cargo.toml` itself from `plugin.toml`: crates.io
//!   dependencies by version, nothing else. No `build.rs`, `[patch]`, path or
//!   git dependency, or profile setting can come from the source.
//! - Only `src/**/*.rs` (no symlinks, size-capped), `plugin.toml` and
//!   `Cargo.lock` are copied into a clean folder of Clew's to build in: a
//!   `.cargo/config.toml` or `rust-toolchain.toml` in the source never
//!   reaches cargo.
//! - cargo and rustc are run by absolute path, told only the variables
//!   finding the toolchain and linking need ([`kept`]): nothing that steers
//!   a build (`RUSTFLAGS`, wrappers), and nothing of the user's — a key in
//!   a variable — for `env!` to read. The network's settings are told only
//!   to `cargo fetch`, which downloads and runs nothing; what compiles runs
//!   offline.
//! - Before building, `cargo metadata` lists every package with a build
//!   script or that is a proc macro: the code a build runs besides the
//!   compiler. Each must be one of a few crates nearly every Rust program
//!   uses ([`BUILD_CODE_ALLOWED`]), or the build is refused, naming it. The
//!   plugin's own crate and the SDK are known by where they are, not by
//!   name: a crates.io crate named like them is checked as any other.
//! - Nothing compiled reads a file of the computer's beyond its own sources,
//!   or its environment (`reads`): the plugin's code may not include files
//!   or read variables at all, and a dependency only includes files of its
//!   own package.
//!
//! What comes back for an agent is short: the compiler's own messages (20 at
//! most, no colour, paths as in the source), or what the plugin is.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::inspect::{Inspected, embed, inspect};
use crate::manifest::{Manifest, permission_names};

/// The target plugins are built for.
pub const TARGET: &str = "wasm32-wasip2";

/// The oldest Rust that builds for [`TARGET`] as a supported target.
pub const MIN_RUST: (u32, u32) = (1, 82);

/// Crates whose build script or proc macro a plugin's build may run: the
/// ones nearly every Rust program depends on, through serde, serde_json and
/// url among others. Any other is refused. None of the proc macros can put a
/// macro's name together from parts (as `paste` can), which would hide an
/// `include_str!` from the check of the plugin's own code (`reads`).
pub const BUILD_CODE_ALLOWED: &[&str] = &[
    "anyhow",
    "autocfg",
    "displaydoc",
    "icu_normalizer_data",
    "icu_properties_data",
    "libc",
    "libm",
    "memchr",
    "num-traits",
    "proc-macro2",
    "quote",
    "rustversion",
    "serde",
    "serde_core",
    "serde_derive",
    "serde_json",
    "syn",
    "thiserror",
    "thiserror-impl",
    "typenum",
    "version_check",
    "yoke-derive",
    "zerocopy",
    "zerocopy-derive",
    "zerofrom-derive",
    "zerovec-derive",
    "zmij",
];

/// The guest SDK, written next to every plugin built.
const SDK: [(&str, &str); 4] = [
    (
        "Cargo.toml",
        include_str!("../../clew-plugin-sdk/Cargo.toml"),
    ),
    (
        "src/lib.rs",
        include_str!("../../clew-plugin-sdk/src/lib.rs"),
    ),
    ("src/rt.rs", include_str!("../../clew-plugin-sdk/src/rt.rs")),
    (
        "src/bindings.rs",
        include_str!("../../clew-plugin-sdk/src/bindings.rs"),
    ),
];

/// The limits on a plugin's source.
const MAX_SOURCE_FILES: usize = 200;
const MAX_SOURCE_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SOURCE_DEPTH: usize = 8;

/// How long resolving dependencies, and the build, may take (a first build
/// downloads the crates it uses).
const METADATA_DEADLINE: Duration = Duration::from_secs(5 * 60);
const BUILD_DEADLINE: Duration = Duration::from_secs(10 * 60);

/// The most compiler messages handed back.
const MAX_MESSAGES: usize = 20;

/// The Rust a plugin is built with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    pub cargo: PathBuf,
    pub rustc: PathBuf,
    /// `rustc`'s version, as `1.95.0`.
    pub version: String,
    /// Where cargo keeps what it downloads, for Clew's own Rust
    /// (`toolchain::managed_dir`); the user's default otherwise.
    pub cargo_home: Option<PathBuf>,
}

/// Why a build gave no plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// No Rust that can build plugins: what is missing, and what fixes it
    /// if anything does (a sentence that follows "To fix it: ").
    Toolchain {
        message: String,
        fix: Option<String>,
    },
    /// `plugin.toml` or the source as a whole is not right.
    Source(String),
    /// A dependency would run code of its own while building, or the code
    /// would read what it may not.
    Policy(String),
    /// The compiler refused the code: its messages.
    Compile(Vec<String>),
    /// cargo could not run, or did not finish.
    Failed(String),
}

impl BuildError {
    /// The error as an agent reads it.
    pub fn report(&self) -> String {
        match self {
            BuildError::Toolchain { message, fix } => match fix {
                Some(fix) => format!("{message}\nTo fix it: {fix}"),
                None => message.clone(),
            },
            BuildError::Source(why) => format!("The plugin's source is not right: {why}"),
            BuildError::Policy(why) => why.clone(),
            BuildError::Compile(messages) => {
                format!("The plugin did not compile:\n\n{}", messages.join("\n"))
            }
            BuildError::Failed(why) => format!("The build did not finish: {why}"),
        }
    }
}

/// A plugin built: its file, as an install would read it.
#[derive(Debug, Clone)]
pub struct Built {
    pub bytes: Vec<u8>,
    pub inspected: Inspected,
    /// Where the file is.
    pub artifact: PathBuf,
    /// What the compiler warned of in the plugin's own code, and what does
    /// not add up (a permission listed that the code never uses).
    pub warnings: Vec<String>,
}

// -- the toolchain -------------------------------------------------------------

/// The toolchain whose programs are `cargo` and `rustc`, checked: recent
/// enough, and with the [`TARGET`] standard library (`rustup`: whether
/// rustup manages it, so the fix is a rustup command).
pub fn toolchain_from(
    cargo: PathBuf,
    rustc: PathBuf,
    rustup: bool,
) -> Result<Toolchain, BuildError> {
    let version = rustc_version(&rustc)?;
    let sysroot = output(Command::new(&rustc).args(["--print", "sysroot"]))
        .map_err(|e| toolchain(format!("could not ask rustc for its sysroot: {e}")))?;
    let std_lib = Path::new(sysroot.trim())
        .join("lib")
        .join("rustlib")
        .join(TARGET);
    // Its standard library, not only its folder: an install stopped
    // partway leaves the folder.
    let has_std = std::fs::read_dir(std_lib.join("lib")).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("libstd-") && name.ends_with(".rlib")
        })
    });
    if !has_std {
        return Err(BuildError::Toolchain {
            message: format!(
                "Rust {version} is installed, but not its {TARGET} target, which plugins are built for."
            ),
            fix: rustup.then(|| format!("run `rustup target add {TARGET}`.")),
        });
    }
    Ok(Toolchain {
        cargo,
        rustc,
        version,
        cargo_home: None,
    })
}

fn toolchain(message: String) -> BuildError {
    BuildError::Toolchain { message, fix: None }
}

/// `rustc`'s version (`1.95.0`), or why it said none.
pub(crate) fn rustc_version_of(rustc: &Path) -> Result<String, String> {
    let out =
        output(Command::new(rustc).arg("-vV")).map_err(|e| format!("could not run rustc: {e}"))?;
    out.lines()
        .find_map(|l| l.strip_prefix("release: "))
        .map(|v| v.trim().to_string())
        .ok_or_else(|| "rustc did not say its version".into())
}

/// Whether Rust `version` is older than [`MIN_RUST`].
pub(crate) fn too_old(version: &str) -> bool {
    let mut parts = version
        .split(['.', '-'])
        .map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) < MIN_RUST
}

/// `rustc`'s version, refused if older than [`MIN_RUST`].
fn rustc_version(rustc: &Path) -> Result<String, BuildError> {
    let version = rustc_version_of(rustc).map_err(toolchain)?;
    if too_old(&version) {
        return Err(BuildError::Toolchain {
            message: format!(
                "Rust {version} is too old to build plugins; they need {}.{} or later.",
                MIN_RUST.0, MIN_RUST.1
            ),
            fix: Some("run `rustup update stable`.".into()),
        });
    }
    Ok(version)
}

/// A short command's standard output, or why it failed.
fn output(cmd: &mut Command) -> Result<String, String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

// -- the source ----------------------------------------------------------------

/// `plugin.toml` in `source`, read and checked.
pub fn read_manifest(source: &Path) -> Result<Manifest, BuildError> {
    let path = source.join("plugin.toml");
    let text = match clew_core::statefile::read_capped_checked(
        &path,
        crate::manifest::MAX_MANIFEST_BYTES as u64,
    ) {
        Ok(Some(text)) => text,
        Ok(None) => return Err(BuildError::Source("there is no plugin.toml".into())),
        Err(e) => return Err(BuildError::Source(format!("plugin.toml: {e}"))),
    };
    Manifest::parse(&text).map_err(BuildError::Source)
}

/// The `Cargo.toml` Clew builds `manifest`'s plugin with.
pub fn cargo_toml(manifest: &Manifest) -> String {
    let mut package = toml::Table::new();
    package.insert("name".into(), manifest.plugin.id.clone().into());
    package.insert("version".into(), manifest.plugin.version.clone().into());
    package.insert("edition".into(), "2021".into());
    package.insert("publish".into(), false.into());
    // Only the library: no build script, and nothing else cargo would find
    // and build by itself.
    package.insert("build".into(), false.into());
    for auto in ["autobins", "autoexamples", "autotests", "autobenches"] {
        package.insert(auto.into(), false.into());
    }
    let mut lib = toml::Table::new();
    lib.insert("path".into(), "src/lib.rs".into());
    lib.insert(
        "crate-type".into(),
        toml::Value::Array(vec!["cdylib".into()]),
    );
    let mut dependencies = toml::Table::new();
    let mut sdk = toml::Table::new();
    sdk.insert("path".into(), "../sdk/clew-plugin-sdk".into());
    dependencies.insert("clew-plugin-sdk".into(), sdk.into());
    for (name, version) in &manifest.dependencies {
        dependencies.insert(name.clone(), version.clone().into());
    }
    let mut release = toml::Table::new();
    release.insert("opt-level".into(), toml::Value::Integer(3));
    release.insert("debug".into(), false.into());
    release.insert("strip".into(), true.into());
    let mut profile = toml::Table::new();
    profile.insert("release".into(), release.into());
    let mut doc = toml::Table::new();
    doc.insert("package".into(), package.into());
    doc.insert("lib".into(), lib.into());
    doc.insert("dependencies".into(), dependencies.into());
    doc.insert("profile".into(), profile.into());
    // A workspace of its own: cargo looks no further up for one.
    doc.insert("workspace".into(), toml::Table::new().into());
    format!(
        "# Written by Clew from plugin.toml for each build; edit plugin.toml instead.\n{}",
        toml::to_string(&doc).unwrap_or_default()
    )
}

/// Copy `source/src/**/*.rs` into `crate_dir/src`, which is emptied first:
/// no symlinks, nothing but Rust files, within the limits.
fn stage_sources(source: &Path, crate_dir: &Path) -> Result<(), BuildError> {
    let from = source.join("src");
    let meta = std::fs::symlink_metadata(&from)
        .map_err(|_| BuildError::Source("there is no src folder".into()))?;
    if !meta.is_dir() {
        return Err(BuildError::Source("src is not a folder".into()));
    }
    let to = crate_dir.join("src");
    let _ = std::fs::remove_dir_all(&to);
    std::fs::create_dir_all(&to).map_err(|e| BuildError::Failed(e.to_string()))?;
    let mut files = 0usize;
    let mut bytes = 0u64;
    copy_rust(&from, &to, 0, &mut files, &mut bytes)?;
    if !to.join("lib.rs").is_file() {
        return Err(BuildError::Source("there is no src/lib.rs".into()));
    }
    let mut found = Vec::new();
    check_own_code(&to, "src", &mut found);
    if found.is_empty() {
        Ok(())
    } else {
        found.sort();
        found.truncate(MAX_MESSAGES);
        Err(BuildError::Policy(format!(
            "The plugin was not built:\n{}",
            found.join("\n")
        )))
    }
}

/// What the Rust files staged in `dir` (`rel` from the plugin's folder)
/// would read of the computer they are built on.
fn check_own_code(dir: &Path, rel: &str, found: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let rel = format!("{rel}/{name}");
        if path.is_dir() {
            check_own_code(&path, &rel, found);
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            found.extend(crate::reads::own_code(&rel, &text));
        }
    }
}

fn copy_rust(
    from: &Path,
    to: &Path,
    depth: usize,
    files: &mut usize,
    bytes: &mut u64,
) -> Result<(), BuildError> {
    if depth > MAX_SOURCE_DEPTH {
        return Err(BuildError::Source(format!(
            "src is more than {MAX_SOURCE_DEPTH} folders deep"
        )));
    }
    let entries = std::fs::read_dir(from).map_err(|e| BuildError::Failed(e.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|e| BuildError::Failed(e.to_string()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') {
            continue;
        }
        let meta = entry
            .metadata()
            .map_err(|e| BuildError::Failed(e.to_string()))?;
        let file_type = entry
            .file_type()
            .map_err(|e| BuildError::Failed(e.to_string()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let sub = to.join(name);
            std::fs::create_dir_all(&sub).map_err(|e| BuildError::Failed(e.to_string()))?;
            copy_rust(&entry.path(), &sub, depth + 1, files, bytes)?;
        } else if file_type.is_file() && name.ends_with(".rs") {
            if meta.len() > MAX_SOURCE_FILE_BYTES {
                return Err(BuildError::Source(format!(
                    "src/…/{name} is larger than {} KB",
                    MAX_SOURCE_FILE_BYTES / 1024
                )));
            }
            *files += 1;
            *bytes += meta.len();
            if *files > MAX_SOURCE_FILES || *bytes > MAX_SOURCE_BYTES {
                return Err(BuildError::Source(format!(
                    "src holds more than {MAX_SOURCE_FILES} files or {} MB",
                    MAX_SOURCE_BYTES / (1024 * 1024)
                )));
            }
            let text =
                std::fs::read(entry.path()).map_err(|e| BuildError::Failed(e.to_string()))?;
            std::fs::write(to.join(name), text).map_err(|e| BuildError::Failed(e.to_string()))?;
        }
    }
    Ok(())
}

/// Write `contents` to `path` unless it holds them already: cargo then sees
/// the SDK unchanged, and rebuilds nothing of it.
fn write_if_changed(path: &Path, contents: &str) -> std::io::Result<()> {
    if std::fs::read(path).is_ok_and(|now| now == contents.as_bytes()) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, contents)
}

// -- running cargo -------------------------------------------------------------

/// Whether cargo and the compiler are told Clew's variable `name`: what
/// finding the toolchain, its homes and the linker needs, and — for the
/// download only (`network`) — how to reach crates.io. Everything else is
/// left out: what steers a build (`RUSTFLAGS`, wrappers, another target
/// folder or toolchain), and anything of the user's, a key in a variable,
/// which `env!` in compiled code would read.
pub(crate) fn kept(name: &str, network: bool) -> bool {
    const ALWAYS: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "TMPDIR",
        "TEMP",
        "TMP",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "LD_LIBRARY_PATH",
        // macOS: where the linker for build scripts is.
        "DEVELOPER_DIR",
        "SDKROOT",
        "MACOSX_DEPLOYMENT_TARGET",
        // Windows: the system, and where Visual Studio's linker is.
        "SYSTEMROOT",
        "SYSTEMDRIVE",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "PROGRAMDATA",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PROGRAMW6432",
        "COMMONPROGRAMFILES",
        "COMMONPROGRAMFILES(X86)",
        "COMMONPROGRAMW6432",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
        "OS",
        "INCLUDE",
        "LIB",
        "LIBPATH",
        "PLATFORM",
        "UCRTVERSION",
        "UNIVERSALCRTSDKDIR",
        "VCINSTALLDIR",
        "VCTOOLSINSTALLDIR",
        "VCTOOLSVERSION",
        "VSINSTALLDIR",
        "VISUALSTUDIOVERSION",
        "DEVENVDIR",
        "WINDOWSSDKDIR",
        "WINDOWSSDKVERSION",
        "WINDOWSSDKBINPATH",
        "WINDOWSSDKLIBVERSION",
        "WINDOWSLIBPATH",
        "EXTENSIONSDKDIR",
        "FRAMEWORKDIR",
        "FRAMEWORKDIR64",
        "FRAMEWORKVERSION",
        "FRAMEWORKVERSION64",
    ];
    const NETWORK: &[&str] = &[
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "CARGO_HTTP_PROXY",
        "CARGO_HTTP_CAINFO",
        "CARGO_HTTP_CHECK_REVOKE",
        "CARGO_HTTP_TIMEOUT",
        "CARGO_HTTP_LOW_SPEED_LIMIT",
        "CARGO_HTTP_MULTIPLEXING",
        "CARGO_HTTP_SSL_VERSION",
        "CARGO_NET_RETRY",
        "CARGO_NET_GIT_FETCH_WITH_CLI",
        "CARGO_REGISTRIES_CRATES_IO_PROTOCOL",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ];
    let upper = name.to_ascii_uppercase();
    ALWAYS.contains(&upper.as_str())
        // Nix's compiler wrappers read their settings from these.
        || upper.starts_with("NIX_")
        || (network && NETWORK.contains(&upper.as_str()))
}

/// cargo, set up to build in `dir` with `toolchain`, into `target`; told
/// how to reach crates.io only when it downloads (`network`).
fn cargo(toolchain: &Toolchain, dir: &Path, target: &Path, network: bool) -> Command {
    let mut cmd = clew_core::spawn::command(&toolchain.cargo);
    cmd.current_dir(dir).env_clear();
    for (name, value) in std::env::vars_os() {
        if name.to_str().is_some_and(|n| kept(n, network)) {
            cmd.env(name, value);
        }
    }
    cmd.env("RUSTC", &toolchain.rustc)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_TERM_PROGRESS_WHEN", "never");
    if let Some(home) = &toolchain.cargo_home {
        cmd.env("CARGO_HOME", home);
    }
    cmd
}

/// What a finished run of a program left.
pub(crate) struct Ran {
    pub(crate) success: bool,
    pub(crate) stdout: String,
    /// The end of its standard error.
    pub(crate) stderr: String,
}

/// The most of cargo's standard output kept, and of its error.
const STDOUT_CAP: usize = 32 * 1024 * 1024;
const STDERR_TAIL: usize = 64 * 1024;

/// Run `cmd` to its end within `limit`, or until `cancel` is set: then it
/// is stopped with all it started (its own process group).
pub(crate) fn run(
    mut cmd: Command,
    limit: Duration,
    cancel: &AtomicBool,
) -> Result<Ran, BuildError> {
    use std::io::Read;
    if cancel.load(Ordering::Relaxed) {
        return Err(BuildError::Failed("cancelled".into()));
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let program = Path::new(cmd.get_program())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut child = cmd
        .spawn()
        .map_err(|e| BuildError::Failed(format!("could not run {program}: {e}")))?;
    #[cfg(windows)]
    let _job = clew_core::procgroup::adopt_scoped(&child);
    let read = |stream: Option<Box<dyn Read + Send>>, keep_tail: bool, cap: usize| {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        if let Some(mut stream) = stream {
            std::thread::spawn(move || {
                let mut kept = Vec::new();
                let mut buf = [0u8; 16 * 1024];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if keep_tail {
                        kept.extend_from_slice(&buf[..n]);
                        if kept.len() > 2 * cap {
                            kept.drain(..kept.len() - cap);
                        }
                    } else if kept.len() < cap {
                        let room = cap - kept.len();
                        kept.extend_from_slice(&buf[..n.min(room)]);
                    }
                }
                let _ = tx.send(kept);
            });
        }
        rx
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        false,
        STDOUT_CAP,
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        true,
        STDERR_TAIL,
    );
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let cancelled = cancel.load(Ordering::Relaxed);
                if !cancelled && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                clew_core::procgroup::kill(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(BuildError::Failed(if cancelled {
                    "cancelled".into()
                } else {
                    format!("it took longer than {} minutes", limit.as_secs() / 60)
                }));
            }
            Err(e) => return Err(BuildError::Failed(e.to_string())),
        }
    };
    let text = |rx: std::sync::mpsc::Receiver<Vec<u8>>| {
        String::from_utf8_lossy(&rx.recv().unwrap_or_default()).into_owned()
    };
    Ok(Ran {
        success: status.success(),
        stdout: text(stdout),
        stderr: text(stderr),
    })
}

/// The last `n` lines of cargo's error output that say something.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("Compiling ")
                || t.starts_with("Downloaded ")
                || t.starts_with("Downloading ")
                || t.starts_with("Updating ")
                || t.starts_with("Locking ")
                || t.starts_with("Adding "))
        })
        .collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// `cargo metadata`'s packages, but the plugin's own crate and the SDK:
/// known by being local packages at `ours` (their `Cargo.toml`s), not by
/// their names, which a crates.io crate can have too.
fn dependencies<'m>(
    metadata: &'m serde_json::Value,
    ours: &[PathBuf],
) -> impl Iterator<Item = &'m serde_json::Value> {
    let ours: Vec<PathBuf> = ours
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect();
    metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |package| {
            let local = package["source"].is_null();
            let at = package["manifest_path"].as_str().map(PathBuf::from);
            let at = at.map(|p| p.canonicalize().unwrap_or(p));
            !(local && at.is_some_and(|at| ours.contains(&at)))
        })
}

fn read_metadata(metadata: &str) -> Result<serde_json::Value, BuildError> {
    serde_json::from_str(metadata)
        .map_err(|e| BuildError::Failed(format!("cargo metadata said what Clew cannot read: {e}")))
}

/// Refuse a build whose dependencies run code of their own while building,
/// beyond [`BUILD_CODE_ALLOWED`], as `cargo metadata` lists them.
fn check_dependencies(metadata: &str, ours: &[PathBuf]) -> Result<(), BuildError> {
    let metadata = read_metadata(metadata)?;
    let mut refused = Vec::new();
    for package in dependencies(&metadata, ours) {
        let name = package["name"].as_str().unwrap_or_default();
        let runs_code = package["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| {
                t["kind"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|k| matches!(k.as_str(), Some("custom-build") | Some("proc-macro")))
            });
        if runs_code && !BUILD_CODE_ALLOWED.contains(&name) {
            let version = package["version"].as_str().unwrap_or_default();
            refused.push(format!("{name} {version}"));
        }
    }
    if refused.is_empty() {
        Ok(())
    } else {
        refused.sort();
        refused.dedup();
        Err(BuildError::Policy(format!(
            "The plugin was not built: {} would run code of its own while building (a build script or a proc macro). A plugin may only use crates that do not, apart from {}. Use another crate, or write what you need yourself.",
            refused.join(", "),
            BUILD_CODE_ALLOWED.join(", ")
        )))
    }
}

/// The version of [`check_dependency_reads`]'s rules: what passed under
/// other rules is checked again.
const READS_RULES: &str = "reads 1";

/// Refuse a build in which a dependency would read a file outside its own
/// package (`reads`). A crates.io package is the same wherever it is used,
/// so one that passed is noted in `checked` and not read again.
fn check_dependency_reads(
    metadata: &str,
    ours: &[PathBuf],
    checked: &Path,
) -> Result<(), BuildError> {
    let metadata = read_metadata(metadata)?;
    let before = std::fs::read_to_string(checked).unwrap_or_default();
    let mut lines = before.lines();
    let mut passed: std::collections::BTreeSet<String> = if lines.next() == Some(READS_RULES) {
        lines.map(str::to_string).collect()
    } else {
        Default::default()
    };
    let count = passed.len();
    let mut refused = Vec::new();
    for package in dependencies(&metadata, ours) {
        let id = package["id"].as_str().unwrap_or_default();
        let from_registry = package["source"]
            .as_str()
            .is_some_and(|s| s.starts_with("registry+") || s.starts_with("sparse+"));
        if from_registry && passed.contains(id) {
            continue;
        }
        let name = package["name"].as_str().unwrap_or_default();
        let version = package["version"].as_str().unwrap_or_default();
        let Some(root) = package["manifest_path"]
            .as_str()
            .and_then(|m| Path::new(m).parent())
        else {
            continue;
        };
        let libs = package["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| {
                t["kind"].as_array().into_iter().flatten().any(|k| {
                    matches!(
                        k.as_str(),
                        Some("lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro")
                    )
                })
            })
            .filter_map(|t| t["src_path"].as_str());
        let mut ok = true;
        for lib in libs {
            if let Err(why) = crate::reads::dependency(root, Path::new(lib)) {
                refused.push(format!("{name} {version}: {why}"));
                ok = false;
                break;
            }
        }
        if ok && from_registry {
            passed.insert(id.to_string());
        }
    }
    if passed.len() != count {
        let mut text = format!("{READS_RULES}\n");
        for id in &passed {
            text.push_str(id);
            text.push('\n');
        }
        let _ = std::fs::write(checked, text);
    }
    if refused.is_empty() {
        return Ok(());
    }
    refused.sort();
    refused.truncate(MAX_MESSAGES);
    Err(BuildError::Policy(format!(
        "The plugin was not built: a dependency would read a file outside its own package while building, which could be one of the user's.\n{}\nUse another crate, or write what you need yourself.",
        refused.join("\n")
    )))
}

/// What `cargo build --message-format=json` said: the plugin's file, the
/// errors, and the warnings in the plugin's own code.
struct Messages {
    artifact: Option<PathBuf>,
    errors: Vec<String>,
    warnings: Vec<String>,
}

fn read_messages(stdout: &str, plugin: &str) -> Messages {
    let lib = plugin.replace('-', "_");
    let mut messages = Messages {
        artifact: None,
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match message["reason"].as_str() {
            Some("compiler-artifact") if message["target"]["name"] == lib.as_str() => {
                messages.artifact = message["filenames"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| f.as_str())
                    .find(|f| f.ends_with(".wasm"))
                    .map(PathBuf::from);
            }
            Some("compiler-message") => {
                let rendered = message["message"]["rendered"].as_str().unwrap_or_default();
                let rendered = rendered.trim_end().to_string();
                if rendered.is_empty() {
                    continue;
                }
                let own = message["target"]["name"] == lib.as_str();
                match message["message"]["level"].as_str() {
                    Some("error") | Some("error: internal compiler error")
                        if messages.errors.len() < MAX_MESSAGES
                            && !rendered.starts_with("error: aborting due to") =>
                    {
                        messages.errors.push(capped(&rendered, 8 * 1024));
                    }
                    Some("warning")
                        if own
                            && messages.warnings.len() < MAX_MESSAGES
                            && !rendered.starts_with("warning: unused")
                            && !rendered.contains("warning emitted")
                            && !rendered.contains("warnings emitted") =>
                    {
                        messages.warnings.push(capped(&rendered, 4 * 1024));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    messages
}

fn capped(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…", &text[..end])
}

// -- the build ------------------------------------------------------------------

/// Build the plugin whose source is `source` with `toolchain`, in `work`
/// (Clew's own build folder: `<data>/plugins/build`). The result is checked
/// as an install would check it: a plugin whose code needs a permission its
/// `plugin.toml` does not list is refused here, with which one.
pub fn build(
    source: &Path,
    work: &Path,
    toolchain: &Toolchain,
    cancel: &AtomicBool,
) -> Result<Built, BuildError> {
    let id = read_manifest(source)?.plugin.id;
    let crate_dir = work.join(&id);
    std::fs::create_dir_all(&crate_dir).map_err(|e| BuildError::Failed(e.to_string()))?;
    let _exclusive = clew_core::statefile::lock_named(&crate_dir.join("Cargo.toml"), ".build.lock")
        .map_err(|e| BuildError::Failed(format!("another build of {id} is running ({e})")))?;

    for (name, contents) in SDK {
        write_if_changed(&work.join("sdk/clew-plugin-sdk").join(name), contents)
            .map_err(|e| BuildError::Failed(e.to_string()))?;
    }
    // What this build is made from, read once the build is this one's and
    // before anything of the source is: a write meanwhile makes the source
    // newer than the build, never the build newer than it says. Read
    // before the lock, a plugin.toml written while another build held it
    // was built in its old words and recorded in its new ones.
    let made_from = source_digest(source);
    let manifest = read_manifest(source)?;
    if manifest.plugin.id != id {
        return Err(BuildError::Failed(format!(
            "plugin.toml changed its id from \"{id}\" to \"{}\" while the build waited: build again",
            manifest.plugin.id
        )));
    }
    stage_sources(source, &crate_dir)?;
    write_if_changed(&crate_dir.join("Cargo.toml"), &cargo_toml(&manifest))
        .map_err(|e| BuildError::Failed(e.to_string()))?;
    let lock = source.join("Cargo.lock");
    match std::fs::read(&lock) {
        Ok(bytes) if bytes.len() <= 1024 * 1024 && !lock.is_symlink() => {
            std::fs::write(crate_dir.join("Cargo.lock"), bytes)
                .map_err(|e| BuildError::Failed(e.to_string()))?;
        }
        _ => {
            let _ = std::fs::remove_file(crate_dir.join("Cargo.lock"));
        }
    }

    let target = work.join("target");
    // Downloading is all that reaches the network, and runs nothing.
    let mut fetch = cargo(toolchain, &crate_dir, &target, true);
    fetch.arg("fetch");
    let ran = run(fetch, METADATA_DEADLINE, cancel)?;
    if !ran.success {
        return Err(BuildError::Source(format!(
            "its dependencies could not be resolved or downloaded:\n{}",
            last_lines(&ran.stderr, 20)
        )));
    }
    let mut metadata = cargo(toolchain, &crate_dir, &target, false);
    metadata.args([
        "metadata",
        "--offline",
        "--format-version",
        "1",
        "--filter-platform",
        TARGET,
    ]);
    let ran = run(metadata, METADATA_DEADLINE, cancel)?;
    if !ran.success {
        return Err(BuildError::Source(format!(
            "its dependencies could not be resolved:\n{}",
            last_lines(&ran.stderr, 20)
        )));
    }
    let ours = [
        crate_dir.join("Cargo.toml"),
        work.join("sdk/clew-plugin-sdk/Cargo.toml"),
    ];
    check_dependencies(&ran.stdout, &ours)?;
    check_dependency_reads(&ran.stdout, &ours, &work.join(".checked-dependencies"))?;

    let mut build = cargo(toolchain, &crate_dir, &target, false);
    build.args([
        "build",
        "--offline",
        "--release",
        "--target",
        TARGET,
        "--message-format=json",
    ]);
    let ran = run(build, BUILD_DEADLINE, cancel)?;
    let messages = read_messages(&ran.stdout, &id);
    if !ran.success {
        if !messages.errors.is_empty() {
            return Err(BuildError::Compile(messages.errors));
        }
        return Err(BuildError::Failed(last_lines(&ran.stderr, 30)));
    }
    let wasm = messages
        .artifact
        .ok_or_else(|| BuildError::Failed("cargo built no WebAssembly file".into()))?;
    let component = std::fs::read(&wasm).map_err(|e| BuildError::Failed(e.to_string()))?;
    let bytes = embed(&component, &manifest).map_err(BuildError::Failed)?;
    let inspected = inspect(&bytes).map_err(BuildError::Source)?;

    let mut warnings = messages.warnings;
    let unused = manifest.declared() & !inspected.needs;
    if unused != 0 {
        warnings.push(format!(
            "plugin.toml lists {} in [plugin] permissions, but the code never uses {}: take {} out, so the user is not asked for it",
            quoted(&permission_names(unused)),
            if unused.count_ones() == 1 { "it" } else { "them" },
            if unused.count_ones() == 1 { "it" } else { "them" },
        ));
    }
    let artifact = crate_dir.join(format!("{id}.wasm"));
    std::fs::write(&artifact, &bytes).map_err(|e| BuildError::Failed(e.to_string()))?;
    // Which source the build is of (`built_from`): a build that fails
    // leaves the last one's, and its source, changed since, no longer
    // matches it.
    record_built_from(&crate_dir, made_from).map_err(|e| BuildError::Failed(e.to_string()))?;
    if let Ok(lock) = std::fs::read(crate_dir.join("Cargo.lock")) {
        let _ = std::fs::write(source.join("Cargo.lock"), lock);
    }
    Ok(Built {
        bytes,
        inspected,
        artifact,
        warnings,
    })
}

/// The file beside a build that names the source it was built from.
const SOURCE_STAMP: &str = "built-from";

/// A digest of what a build of the plugin whose source is `source` is made
/// of: its `plugin.toml` and the Rust files under `src/` that are staged
/// (`stage_sources`), each with its path. `None` when one cannot be read.
pub fn source_digest(source: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    fn rust_files(dir: &Path, rel: &str, depth: usize, out: &mut Vec<(String, PathBuf)>) {
        if depth > MAX_SOURCE_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if name.starts_with('.') || kind.is_symlink() {
                continue;
            }
            let rel = format!("{rel}/{name}");
            if kind.is_dir() {
                rust_files(&entry.path(), &rel, depth + 1, out);
            } else if kind.is_file() && name.ends_with(".rs") {
                out.push((rel, entry.path()));
            }
        }
    }
    let mut files = vec![("plugin.toml".to_string(), source.join("plugin.toml"))];
    let mut rust = Vec::new();
    rust_files(&source.join("src"), "src", 0, &mut rust);
    rust.sort();
    files.extend(rust);
    let mut hash = Sha256::new();
    for (rel, path) in files {
        let bytes = std::fs::read(path).ok()?;
        hash.update((rel.len() as u64).to_le_bytes());
        hash.update(rel.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    Some(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Note beside the build in `crate_dir` the source it was built from
/// (`source_digest`); `None`, when that could not be read, notes none.
pub(crate) fn record_built_from(crate_dir: &Path, digest: Option<String>) -> std::io::Result<()> {
    let stamp = crate_dir.join(SOURCE_STAMP);
    match digest {
        Some(digest) => std::fs::write(&stamp, digest),
        None => std::fs::remove_file(&stamp).or(Ok(())),
    }
}

/// Whether the build in `crate_dir` is of the plugin's source at `source`
/// as it is now: built from it, and nothing written to it since.
pub fn built_from(crate_dir: &Path, source: &Path) -> bool {
    let Ok(stamp) = std::fs::read_to_string(crate_dir.join(SOURCE_STAMP)) else {
        return false;
    };
    source_digest(source).is_some_and(|now| now == stamp.trim())
}

fn quoted(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(" and ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests_support;

    #[test]
    fn a_build_is_of_the_source_as_it_was_built() {
        let root = std::env::temp_dir().join(format!("clew-build-from-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (source, built) = (root.join("source"), root.join("built"));
        std::fs::create_dir_all(source.join("src/parts")).unwrap();
        std::fs::create_dir_all(&built).unwrap();
        std::fs::write(source.join("plugin.toml"), "[plugin]\n").unwrap();
        std::fs::write(source.join("src/lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(source.join("src/parts/b.rs"), "fn b() {}\n").unwrap();
        // Never built.
        assert!(!built_from(&built, &source));
        record_built_from(&built, source_digest(&source)).unwrap();
        assert!(built_from(&built, &source));
        // What a build does not stage does not change it.
        std::fs::write(source.join("src/notes.md"), "notes").unwrap();
        std::fs::write(source.join("src/.hidden.rs"), "fn h() {}").unwrap();
        std::fs::write(source.join("README.md"), "readme").unwrap();
        assert!(built_from(&built, &source));
        // What it does, does: the manifest, a file in a folder, a new file,
        // a file moved, a file gone.
        let changes: [&dyn Fn(); 5] = [
            &|| std::fs::write(source.join("plugin.toml"), "[plugin]\n# more\n").unwrap(),
            &|| std::fs::write(source.join("src/parts/b.rs"), "fn b2() {}\n").unwrap(),
            &|| std::fs::write(source.join("src/c.rs"), "").unwrap(),
            &|| std::fs::rename(source.join("src/parts/b.rs"), source.join("src/b.rs")).unwrap(),
            &|| std::fs::remove_file(source.join("src/parts/b.rs")).unwrap(),
        ];
        for (n, change) in changes.iter().enumerate() {
            let before = source_digest(&source);
            record_built_from(&built, before.clone()).unwrap();
            change();
            assert!(!built_from(&built, &source), "change {n}");
            assert_ne!(source_digest(&source), before, "change {n}");
            // Put back for the next.
            let _ = std::fs::remove_file(source.join("src/c.rs"));
            let _ = std::fs::remove_file(source.join("src/b.rs"));
            std::fs::write(source.join("plugin.toml"), "[plugin]\n").unwrap();
            std::fs::write(source.join("src/parts/b.rs"), "fn b() {}\n").unwrap();
            assert_eq!(source_digest(&source), before, "change {n} put back");
        }
        // A source that cannot be read is no build's.
        std::fs::remove_file(source.join("plugin.toml")).unwrap();
        assert_eq!(source_digest(&source), None);
        assert!(!built_from(&built, &source));
        record_built_from(&built, None).unwrap();
        assert!(!built.join(SOURCE_STAMP).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cargo_toml_holds_only_what_clew_writes() {
        let mut m = tests_support::manifest();
        m.dependencies.insert("regex".into(), "1".into());
        m.dependencies.insert("weird".into(), "1\"\n[patch]".into());
        let text = cargo_toml(&m);
        let doc: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(doc["package"]["name"].as_str(), Some("sample"));
        assert_eq!(doc["package"]["edition"].as_str(), Some("2021"));
        assert_eq!(doc["dependencies"]["regex"].as_str(), Some("1"));
        assert_eq!(
            doc["dependencies"]["weird"].as_str(),
            Some("1\"\n[patch]"),
            "a value is a string, never TOML of its own"
        );
        assert_eq!(
            doc["dependencies"]["clew-plugin-sdk"]["path"].as_str(),
            Some("../sdk/clew-plugin-sdk")
        );
        assert!(doc.contains_key("workspace"));
        assert!(!doc.contains_key("patch"));
        assert_eq!(
            doc["package"]["build"].as_bool(),
            Some(false),
            "no build script"
        );
    }

    #[test]
    fn only_rust_files_under_src_are_staged() {
        let root = std::env::temp_dir().join(format!("clew-plugin-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join("source");
        std::fs::create_dir_all(source.join("src/nested")).unwrap();
        std::fs::create_dir_all(source.join(".cargo")).unwrap();
        std::fs::write(source.join("src/lib.rs"), "// lib").unwrap();
        std::fs::write(source.join("src/nested/more.rs"), "// more").unwrap();
        std::fs::write(source.join("src/notes.txt"), "no").unwrap();
        std::fs::write(source.join("build.rs"), "fn main() {}").unwrap();
        std::fs::write(
            source.join(".cargo/config.toml"),
            "[build]\nrustc-wrapper = \"x\"",
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", source.join("src/passwd.rs")).unwrap();
        let crate_dir = root.join("work/sample");
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(crate_dir.join("src/stale.rs"), "// gone").unwrap();
        stage_sources(&source, &crate_dir).unwrap();
        assert!(crate_dir.join("src/lib.rs").is_file());
        assert!(crate_dir.join("src/nested/more.rs").is_file());
        assert!(!crate_dir.join("src/notes.txt").exists());
        assert!(!crate_dir.join("src/stale.rs").exists());
        assert!(!crate_dir.join("src/passwd.rs").exists());
        assert!(!crate_dir.join("build.rs").exists());
        assert!(!crate_dir.join(".cargo").exists());

        std::fs::remove_file(source.join("src/lib.rs")).unwrap();
        assert!(matches!(
            stage_sources(&source, &crate_dir),
            Err(BuildError::Source(why)) if why.contains("src/lib.rs")
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dependencies_that_run_code_are_refused_unless_well_known() {
        let ours = [
            PathBuf::from("/w/sample/Cargo.toml"),
            PathBuf::from("/w/sdk/clew-plugin-sdk/Cargo.toml"),
        ];
        let metadata = |name: &str, kind: &str| {
            format!(
                r#"{{"packages":[
                    {{"name":"sample","version":"0.1.0","source":null,"manifest_path":"/w/sample/Cargo.toml","targets":[{{"kind":["cdylib"]}}]}},
                    {{"name":"clew-plugin-sdk","version":"0.1.0","source":null,"manifest_path":"/w/sdk/clew-plugin-sdk/Cargo.toml","targets":[{{"kind":["lib"]}}]}},
                    {{"name":"serde_derive","version":"1.0.0","source":"registry+https://github.com/rust-lang/crates.io-index","manifest_path":"/r/serde_derive/Cargo.toml","targets":[{{"kind":["proc-macro"]}}]}},
                    {{"name":"{name}","version":"2.0.0","source":"registry+https://github.com/rust-lang/crates.io-index","manifest_path":"/r/{name}/Cargo.toml","targets":[{{"kind":["{kind}"]}},{{"kind":["lib"]}}]}}
                ]}}"#
            )
        };
        assert!(check_dependencies(&metadata("regex", "lib"), &ours).is_ok());
        assert!(check_dependencies(&metadata("serde", "custom-build"), &ours).is_ok());
        let refused = check_dependencies(&metadata("evil", "custom-build"), &ours).unwrap_err();
        assert!(matches!(&refused, BuildError::Policy(why) if why.contains("evil 2.0.0")));
        assert!(check_dependencies(&metadata("magic", "proc-macro"), &ours).is_err());
        // A crates.io crate named like the plugin, or the SDK, is not
        // theirs: its build script is refused like any other's.
        for named in ["sample", "clew-plugin-sdk"] {
            let refused = check_dependencies(&metadata(named, "custom-build"), &ours).unwrap_err();
            assert!(
                matches!(&refused, BuildError::Policy(why) if why.contains(&format!("{named} 2.0.0"))),
                "{refused:?}"
            );
        }
        // paste can put a macro's name together, an include_str! the
        // check of the plugin's code would not see.
        assert!(!BUILD_CODE_ALLOWED.contains(&"paste"));
    }

    #[test]
    fn a_dependency_reading_outside_its_package_is_refused_and_one_that_does_not_is_noted() {
        let root = std::env::temp_dir().join(format!("clew-build-reads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let package = |name: &str, lib: &str| {
            let dir = root.join(format!("registry/{name}-1.0.0"));
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(dir.join("Cargo.toml"), "").unwrap();
            std::fs::write(dir.join("src/lib.rs"), lib).unwrap();
            format!(
                r#"{{"id":"registry+https://github.com/rust-lang/crates.io-index#{name}@1.0.0","name":"{name}","version":"1.0.0","source":"registry+https://github.com/rust-lang/crates.io-index","manifest_path":"{}","targets":[{{"kind":["lib"],"src_path":"{}"}}]}}"#,
                dir.join("Cargo.toml").display(),
                dir.join("src/lib.rs").display()
            )
        };
        let good = package("good", "pub const V: &str = env!(\"CARGO_PKG_VERSION\");");
        let bad = package(
            "bad",
            "pub const K: &str = include_str!(\"/home/u/.config/clew/config.toml\");",
        );
        let checked = root.join("checked");
        let both = format!(r#"{{"packages":[{good},{bad}]}}"#);
        let refused = check_dependency_reads(&both, &[], &checked).unwrap_err();
        assert!(
            matches!(&refused, BuildError::Policy(why) if why.contains("bad 1.0.0: src/lib.rs:1: include_str!")),
            "{refused:?}"
        );
        let noted = std::fs::read_to_string(&checked).unwrap();
        assert_eq!(
            noted,
            format!(
                "{READS_RULES}\nregistry+https://github.com/rust-lang/crates.io-index#good@1.0.0\n"
            )
        );
        // Noted, it is not read again.
        std::fs::write(
            root.join("registry/good-1.0.0/src/lib.rs"),
            "include!(\"/x\");",
        )
        .unwrap();
        let alone = format!(r#"{{"packages":[{good}]}}"#);
        assert!(check_dependency_reads(&alone, &[], &checked).is_ok());
        // Under other rules it is.
        std::fs::write(
            &checked,
            "reads 0\nregistry+https://github.com/rust-lang/crates.io-index#good@1.0.0\n",
        )
        .unwrap();
        assert!(check_dependency_reads(&alone, &[], &checked).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cargo_is_told_nothing_of_the_users_but_the_toolchain_and_for_downloads_the_network() {
        for name in [
            "PATH",
            "HOME",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "SystemRoot",
            "NIX_CC",
        ] {
            assert!(kept(name, false), "{name}");
        }
        for name in [
            "DEEPSEEK_API_KEY",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "CARGO_REGISTRY_TOKEN",
            "RUSTFLAGS",
            "RUSTC_WRAPPER",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_RUSTFLAGS",
            "RUSTUP_TOOLCHAIN",
        ] {
            assert!(!kept(name, false) && !kept(name, true), "{name}");
        }
        for name in ["HTTPS_PROXY", "https_proxy", "SSL_CERT_FILE"] {
            assert!(kept(name, true) && !kept(name, false), "{name}");
        }
    }

    #[test]
    fn the_cargo_toml_builds_only_the_library() {
        let manifest = Manifest::parse(
            "[plugin]\nid = \"sample\"\nname = \"S\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n[[command]]\nid = \"a\"\ntitle = \"A\"\n",
        )
        .unwrap();
        let doc: toml::Table = cargo_toml(&manifest).parse().unwrap();
        let package = doc["package"].as_table().unwrap();
        for key in [
            "build",
            "autobins",
            "autoexamples",
            "autotests",
            "autobenches",
        ] {
            assert_eq!(package[key].as_bool(), Some(false), "{key}");
        }
    }

    #[test]
    fn the_plugins_own_code_may_not_read_the_computer() {
        let root = std::env::temp_dir().join(format!("clew-build-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join("source");
        std::fs::create_dir_all(source.join("src/views")).unwrap();
        std::fs::write(source.join("src/lib.rs"), "mod views;").unwrap();
        std::fs::write(
            source.join("src/views/mod.rs"),
            "pub const K: &str = include_str!(\"../../../../config.toml\");",
        )
        .unwrap();
        let refused = stage_sources(&source, &root.join("crate")).unwrap_err();
        assert!(
            matches!(&refused, BuildError::Policy(why) if why.contains("src/views/mod.rs:1: `include_str`")),
            "{refused:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn compiler_messages_are_read_from_cargos_json() {
        let stdout = [
            r#"{"reason":"compiler-message","target":{"name":"regex"},"message":{"level":"warning","rendered":"warning: in a dependency"}}"#,
            r#"{"reason":"compiler-message","target":{"name":"todo_authors"},"message":{"level":"warning","rendered":"warning: this is the plugin's own\n --> src/lib.rs:3:5\n"}}"#,
            r#"{"reason":"compiler-message","target":{"name":"todo_authors"},"message":{"level":"error","rendered":"error[E0425]: cannot find value `x`\n --> src/lib.rs:9:1\n"}}"#,
            r#"{"reason":"compiler-message","target":{"name":"todo_authors"},"message":{"level":"error","rendered":"error: aborting due to 1 previous error"}}"#,
            r#"{"reason":"compiler-artifact","target":{"name":"todo_authors"},"filenames":["/w/target/wasm32-wasip2/release/todo_authors.wasm"]}"#,
            "not json",
        ]
        .join("\n");
        let m = read_messages(&stdout, "todo-authors");
        assert_eq!(m.errors.len(), 1);
        assert!(m.errors[0].contains("E0425"));
        assert_eq!(m.warnings.len(), 1);
        assert!(m.warnings[0].contains("plugin's own"));
        assert_eq!(
            m.artifact.unwrap(),
            PathBuf::from("/w/target/wasm32-wasip2/release/todo_authors.wasm")
        );
    }

    #[test]
    fn the_sdk_written_beside_a_plugin_is_this_one() {
        let names: Vec<&str> = SDK.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            ["Cargo.toml", "src/lib.rs", "src/rt.rs", "src/bindings.rs"]
        );
        assert!(SDK[0].1.contains("name = \"clew-plugin-sdk\""));
    }

    #[test]
    fn a_report_says_what_to_do() {
        let e = BuildError::Toolchain {
            message: "no target".into(),
            fix: Some("rustup target add wasm32-wasip2".into()),
        };
        assert!(
            e.report()
                .contains("To fix it: rustup target add wasm32-wasip2")
        );
        let e = BuildError::Compile(vec!["error: one".into(), "error: two".into()]);
        assert!(e.report().contains("error: one\nerror: two"));
    }
}
