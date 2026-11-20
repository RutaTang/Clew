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
//!   finding the toolchain and linking need (`kept`): nothing that steers
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

/// Why the build failed, when `unit` failed before saying what it read: in
/// Clew's own words, as nothing the compiler said is passed on. The plugin
/// itself: a bracket of its own files not closed, as Clew reads them, or
/// where the compiler stopped (`stops`); a crate it uses, which one.
fn unread_failure(
    crate_dir: &Path,
    plugin: &str,
    unit: &str,
    metadata: &str,
    stops: &[String],
) -> BuildError {
    const WITHHELD: &str =
        "The compiler stopped before it said which files it read, so nothing it said is passed on.";
    match package_of(metadata, unit, plugin) {
        Some(package) => BuildError::Failed(format!(
            "compiling {package}, a crate the plugin uses, failed. {WITHHELD} Use another crate in place of it, or another version of it."
        )),
        None => {
            let mut unpaired = Vec::new();
            unpaired_in(&crate_dir.join("src"), "src", &mut unpaired);
            unpaired.sort();
            let said = match (unpaired.first(), stops.first()) {
                (Some(unpaired), _) => format!("error: {unpaired}"),
                (None, Some(stop)) => match unparsed_at(crate_dir, stop) {
                    Some(why) => format!("error: {why}"),
                    None => {
                        format!("error: {stop}: the compiler stopped here. Look at the code there.")
                    }
                },
                (None, None) => "error: the plugin did not compile.".to_string(),
            };
            BuildError::Compile(vec![format!("{said}\n{WITHHELD}")])
        }
    }
}

/// Why the plugin's file does not parse as Rust, as Clew reads it, where
/// the compiler stopped (`stop`, `file:line:column`): `file:line:column:
/// what was expected`, in words made of the file's own code only. None when
/// Clew reads it as Rust there (the compiler stopped for another reason).
fn unparsed_at(crate_dir: &Path, stop: &str) -> Option<String> {
    let mut parts = stop.rsplitn(3, ':');
    let (_, line, file) = (parts.next()?, parts.next()?, parts.next()?);
    let line: usize = line.parse().ok()?;
    // One of the plugin's own files, staged under `src/` (rustc writes
    // `\\` between the parts on Windows).
    let file = file.replace('\\', "/");
    if !file.starts_with("src/") || file.split('/').any(|part| part == ".." || part.is_empty()) {
        return None;
    }
    let text = std::fs::read_to_string(crate_dir.join(&file)).ok()?;
    // syn descends once per nested expression or type: a file of deep
    // nesting would overflow a thread's stack, and a huge one would take
    // long. Such a file gets the compiler's own words only.
    if crate::reads::tokens(&text) > PARSED_TOKENS {
        return None;
    }
    let (at, column, err) = std::thread::Builder::new()
        .stack_size(PARSE_STACK)
        .spawn(move || {
            syn::parse_file(&text).err().map(|err| {
                let at = err.span().start();
                (at.line, at.column, err.to_string())
            })
        })
        .ok()?
        .join()
        .ok()??;
    (at == line).then(|| format!("{file}:{at}:{}: {err}", column + 1))
}

/// The most tokens in a file Clew parses itself to say where it stops (a
/// file of several hundred lines).
const PARSED_TOKENS: usize = 8_000;

/// The stack Clew parses a file on. Each token opens at most one level of
/// nesting, and the deepest syn goes for one token is about 27 KiB, for a
/// `&` before a type, unoptimized. This leaves 64 KiB a token; it is only
/// reserved, its pages taken as a parse goes deep.
const PARSE_STACK: usize = 512 * 1024 * 1024;

/// Each Rust file staged in `dir` (`rel` from the plugin's folder) whose
/// brackets do not pair up, where ([`crate::reads::unpaired`]).
fn unpaired_in(dir: &Path, rel: &str, found: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let rel = format!("{rel}/{name}");
        if path.is_dir() {
            unpaired_in(&path, &rel, found);
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            found.extend(crate::reads::unpaired(&rel, &text));
        }
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
    // Nix's compiler and linker wrappers read their settings from these;
    // `NIX_CONFIG` and the like, which can hold a token, are not among them.
    const NIX: &[&str] = &[
        "NIX_CC",
        "NIX_BINTOOLS",
        "NIX_CFLAGS",
        "NIX_LDFLAGS",
        "NIX_HARDENING",
        "NIX_ENFORCE",
        "NIX_DONT_SET_RPATH",
        "NIX_NO_SELF_RPATH",
        "NIX_IGNORE_LD_THROUGH_GCC",
        "NIX_COREFOUNDATION_RPATH",
        "NIX_STORE",
    ];
    let upper = name.to_ascii_uppercase();
    ALWAYS.contains(&upper.as_str())
        || NIX.iter().any(|prefix| upper.starts_with(prefix))
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
        // Where rustc writes what it made and what it read (its `.d`
        // files, a build script's `OUT_DIR`): the plugin's own folder,
        // which is checked and wiped — not one a configuration's
        // `build.build-dir` moved out of it. Cargo's own copy of a `.d`
        // names files from the plugin's folder, as the check reads it.
        .env("CARGO_BUILD_BUILD_DIR", target)
        .env("CARGO_BUILD_DEP_INFO_BASEDIR", dir)
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_TERM_PROGRESS_WHEN", "never")
        // A cargo configuration of the user's (`~/.cargo/config.toml`, read
        // wherever the build is) gives a plugin's build no flags and no
        // wrapper. Set, even empty, these come before any configuration.
        .env("CARGO_ENCODED_RUSTFLAGS", "")
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "");
    if let Some(home) = &toolchain.cargo_home {
        cmd.env("CARGO_HOME", home);
    }
    cmd
}

/// The variables a cargo configuration's `[env]` would set for a build in
/// `dir`, as cargo finds its configuration: `.cargo/config.toml` (or
/// `.cargo/config`) in `dir` and each folder above it, and in cargo's own
/// folder. One already set comes before `[env]`, so each is set empty for
/// a plugin's build; one marked `force = true` would come before that, so
/// the build is refused, naming it.
fn configured_env(dir: &Path, cargo_home: Option<&Path>) -> Result<Vec<String>, BuildError> {
    let home = cargo_home.map(Path::to_path_buf).or_else(|| {
        std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| clew_core::home_dir().map(|h| h.join(".cargo")))
    });
    let mut folders: Vec<PathBuf> = dir.ancestors().map(|a| a.join(".cargo")).collect();
    folders.extend(home);
    let mut names = Vec::new();
    for folder in folders {
        for file in [folder.join("config.toml"), folder.join("config")] {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let Ok(config) = text.parse::<toml::Table>() else {
                continue;
            };
            let Some(env) = config.get("env").and_then(|e| e.as_table()) else {
                continue;
            };
            for (name, value) in env {
                let forced = value
                    .as_table()
                    .and_then(|t| t.get("force"))
                    .and_then(|f| f.as_bool())
                    .unwrap_or(false);
                if forced {
                    return Err(BuildError::Policy(format!(
                        "The plugin was not built: the cargo configuration {} sets {name} for every build (force = true), and a plugin's build may not be given the user's variables. Ask the user whether to take force = true out.",
                        file.display()
                    )));
                }
                names.push(name.clone());
            }
        }
    }
    Ok(names)
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
const READS_RULES: &str = "reads 3";

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
        "The plugin was not built: a dependency names a file outside its own package for the compiler to read, which could be one of the user's.\n{}\nUse another crate, or write what you need yourself.",
        refused.join("\n")
    )))
}

/// What `cargo build --message-format=json` said: the plugin's file, the
/// errors, and the warnings in the plugin's own code.
struct Messages {
    artifact: Option<PathBuf>,
    errors: Vec<String>,
    warnings: Vec<String>,
    /// Every file the compiler's messages point into, with the crate each
    /// message is of (`outside_read`).
    files: Vec<(String, String)>,
    /// The dep-info files (`.d`) of every unit the build used, built now
    /// or before (`outside_read`).
    dep_infos: Vec<PathBuf>,
    /// The units an error was said of ([`unit_of`]), each with the file
    /// it is compiled from (its `target.src_path`): one whose compiling
    /// wrote no dep-info file naming that file said nothing of what it read
    /// (`unread_failure`).
    failed: Vec<(String, Option<String>)>,
    /// Where in the plugin's own files its errors are, `file:line:column`.
    stops: Vec<String>,
}

/// The name a unit of the build goes by, alike in a compiler message and in
/// the name of its dep-info file ([`crate_of`]): its crate's, `-` read as
/// `_`; a build script's, its package's.
fn unit_of(message: &serde_json::Value) -> String {
    let build_script = message["target"]["kind"]
        .as_array()
        .is_some_and(|kinds| kinds.iter().any(|k| k == "custom-build"));
    let name = match build_script {
        // `registry+…#serde@1.0.228`, or before cargo 1.77 `serde 1.0.228 (…)`.
        true => {
            let id = message["package_id"].as_str().unwrap_or_default();
            match id.rsplit_once('#') {
                Some((_, rest)) => rest.split('@').next().unwrap_or_default(),
                None => id.split(' ').next().unwrap_or_default(),
            }
        }
        false => message["target"]["name"].as_str().unwrap_or_default(),
    };
    name.replace('-', "_")
}

fn read_messages(stdout: &str, plugin: &str) -> Messages {
    let lib = plugin.replace('-', "_");
    let mut messages = Messages {
        artifact: None,
        errors: Vec::new(),
        warnings: Vec::new(),
        files: Vec::new(),
        dep_infos: Vec::new(),
        failed: Vec::new(),
        stops: Vec::new(),
    };
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match message["reason"].as_str() {
            Some("compiler-artifact") => {
                let filenames = message["filenames"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| f.as_str());
                for file in filenames.clone() {
                    messages.dep_infos.extend(dep_info_of(Path::new(file)));
                }
                if message["target"]["name"] == lib.as_str() {
                    messages.artifact = filenames
                        .clone()
                        .find(|f| f.ends_with(".wasm"))
                        .map(PathBuf::from);
                }
            }
            Some("compiler-message") => {
                let mut files = Vec::new();
                span_files(&message["message"], &mut files);
                let of = message["target"]["name"].as_str().unwrap_or_default();
                messages
                    .files
                    .extend(files.into_iter().map(|file| (of.to_string(), file)));
                let own = message["target"]["name"] == lib.as_str();
                if message["message"]["level"]
                    .as_str()
                    .is_some_and(|level| level.starts_with("error"))
                {
                    let unit = (
                        unit_of(&message),
                        message["target"]["src_path"].as_str().map(str::to_string),
                    );
                    if !messages.failed.contains(&unit) {
                        messages.failed.push(unit);
                    }
                    let primary = message["message"]["spans"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|s| s["is_primary"] == true);
                    if let Some(span) = primary
                        && own
                        && let Some(file) = span["file_name"].as_str()
                        && Path::new(file).is_relative()
                        && messages.stops.len() < MAX_MESSAGES
                    {
                        messages.stops.push(format!(
                            "{file}:{}:{}",
                            span["line_start"], span["column_start"]
                        ));
                    }
                }
                let rendered = message["message"]["rendered"].as_str().unwrap_or_default();
                let rendered = rendered.trim_end().to_string();
                if rendered.is_empty() {
                    continue;
                }
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

// -- what the compiler read ---------------------------------------------------

/// Every `file_name` a compiler message's spans name, its notes' and the
/// macro expansions they came through included.
fn span_files(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                match (key.as_str(), value) {
                    ("file_name", serde_json::Value::String(file)) => out.push(file.clone()),
                    _ => span_files(value, out),
                }
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| span_files(v, out)),
        _ => {}
    }
}

/// The dep-info file (`.d`) rustc wrote beside `artifact`, one of the
/// files a unit of the build made: `libname-hash.rlib` has `name-hash.d`,
/// `name.wasm` has `name.d`, and a build script's folder holds its own.
fn dep_info_of(artifact: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(stem)) = (artifact.parent(), artifact.file_stem()) else {
        return Vec::new();
    };
    let stem = stem.to_string_lossy();
    if stem == "build-script-build" {
        return std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "d"))
            .collect();
    }
    let stem = stem.strip_prefix("lib").unwrap_or(&stem);
    // rustc's own, in `deps`, besides cargo's copy beside what it uplifted
    // (whose paths a configuration can make relative to elsewhere).
    [
        dir.join(format!("{stem}.d")),
        dir.join("deps").join(format!("{stem}.d")),
    ]
    .into_iter()
    .filter(|file| file.is_file())
    .collect()
}

/// A dep-info file's words, and when it was last written.
type DepInfo = (Vec<u8>, Option<std::time::SystemTime>);

/// The dep-info files under `target`, each with what it says and when.
fn dep_infos(target: &Path) -> std::collections::HashMap<PathBuf, DepInfo> {
    fn walk(dir: &Path, depth: usize, out: &mut std::collections::HashMap<PathBuf, DepInfo>) {
        if depth > 6 || out.len() > 50_000 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            if kind.is_dir() {
                // What build scripts wrote, cargo's records, and rustc's
                // caches are no dep-info of a unit.
                if !matches!(name.to_str(), Some("out" | ".fingerprint" | "incremental")) {
                    walk(&path, depth + 1, out);
                }
            } else if kind.is_file()
                && path.extension().is_some_and(|e| e == "d")
                && let Ok(bytes) = std::fs::read(&path)
            {
                let at = entry.metadata().and_then(|m| m.modified()).ok();
                out.insert(path, (bytes, at));
            }
        }
    }
    let mut out = std::collections::HashMap::new();
    walk(target, 0, &mut out);
    out
}

/// The dep-info files this build wrote: new, or saying other than they
/// did before it (`before`). Those of units that failed are among them, and
/// cargo lists no artifact of those. One written again word for word names
/// what was checked already: every one there before was (`UNCHECKED`).
fn written_dep_infos(
    target: &Path,
    before: &std::collections::HashMap<PathBuf, DepInfo>,
) -> Vec<PathBuf> {
    let mut written: Vec<PathBuf> = dep_infos(target)
        .into_iter()
        .filter(|(path, (bytes, _))| before.get(path).map(|(was, _)| was) != Some(bytes))
        .map(|(path, _)| path)
        .collect();
    written.sort();
    written
}

/// The dep-info files this build wrote, word for word as before or not:
/// new, or written since (their time of writing changed). A failing crate
/// built again unchanged writes its own again, the same.
fn rewritten_dep_infos(
    target: &Path,
    before: &std::collections::HashMap<PathBuf, DepInfo>,
) -> Vec<PathBuf> {
    dep_infos(target)
        .into_iter()
        .filter(|(path, (_, at))| before.get(path).is_none_or(|(_, was)| was != at))
        .map(|(path, _)| path)
        .collect()
}

/// In a plugin's build folder while the build runs, until what it read is
/// checked: one left there is of a build stopped before then (cancelled,
/// out of time, Clew closed), and nothing it compiled is built on.
const UNCHECKED: &str = ".unchecked";

/// The file a dep-info file says its crate is compiled from: the first one
/// its first rule names after what was made (rustc names the crate's root
/// first, the files it reads after).
fn dep_info_root(text: &str) -> Option<String> {
    let line = text.lines().find(|line| !line.is_empty())?;
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                word.push(' ');
                chars.next();
            }
            ' ' | '\t' => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    let made = words.iter().position(|w| w.ends_with(':'))?;
    words.get(made + 1).cloned()
}

/// The files a dep-info file names: every path on its lines, what was made
/// and what it was made from. rustc's `# env-dep:` and `# checksum:` lines
/// name no file; any other line is read for files — a path with a line
/// break in it is not taken for one of those, its rest unread.
fn dep_info_files(text: &str) -> Vec<String> {
    // rustc's `#` lines are the last ones, after an empty line (a nightly's
    // checksums alone may follow no empty line: read for files then, they
    // name none outside). A line so begun anywhere else is a path's,
    // broken by a line break in it.
    let lines: Vec<&str> = text.lines().collect();
    let mut notes = lines.len();
    while notes > 0 && lines[notes - 1].is_empty() {
        notes -= 1;
    }
    let end = notes;
    while notes > 0
        && (lines[notes - 1].starts_with("# env-dep:")
            || lines[notes - 1].starts_with("# checksum:"))
    {
        notes -= 1;
    }
    if notes == end || (notes > 0 && !lines[notes - 1].is_empty()) {
        notes = end;
    }
    let mut out = Vec::new();
    for line in &lines[..notes] {
        let mut word = String::new();
        let mut chars = line.chars().peekable();
        let mut push = |word: &mut String| {
            let file = word.strip_suffix(':').unwrap_or(word);
            if !file.is_empty() {
                out.push(file.to_string());
            }
            word.clear();
        };
        while let Some(c) = chars.next() {
            match c {
                // An escaped space is part of a path.
                '\\' if chars.peek() == Some(&' ') => {
                    word.push(' ');
                    chars.next();
                }
                ' ' | '\t' => push(&mut word),
                _ => word.push(c),
            }
        }
        push(&mut word);
    }
    out
}

/// Where a plugin's build may read: its own folder (the plugin's crate,
/// the SDK, what cargo and the allowed build scripts wrote), each package
/// it depends on, and the toolchain's own library.
struct Readable {
    roots: Vec<PathBuf>,
    /// What a relative path in a message or a dep-info file is from.
    base: PathBuf,
}

impl Readable {
    /// What the build may read: the plugin's own folder (`base`), what the
    /// build made (`target`, where a build script's output is), each
    /// package it uses (the SDK and the crates it names, as cargo's
    /// metadata lists them), and the toolchain's library. Not the rest of
    /// Clew's build folder: another plugin's source is not this one's.
    fn new(target: &Path, base: &Path, metadata: &str, sysroot: Option<PathBuf>) -> Self {
        let mut roots = vec![base.to_path_buf(), target.to_path_buf()];
        // Of the toolchain, its library only (`lib/rustlib`: the standard
        // library's sources and what it compiled): a toolchain installed
        // with `--prefix ~/.local` has all of `~/.local` for its root.
        let rustlib = sysroot.map(|s| s.join("lib").join("rustlib"));
        // Its library's sources, where a link there leads (a toolchain built
        // locally, or a distribution's, links `src/rust` elsewhere).
        let sysroot = rustlib
            .iter()
            .flat_map(|r| [r.clone(), r.join("src").join("rust")])
            .collect::<Vec<_>>();
        if let Ok(metadata) = serde_json::from_str::<serde_json::Value>(metadata) {
            for package in metadata["packages"].as_array().into_iter().flatten() {
                if let Some(root) = package["manifest_path"]
                    .as_str()
                    .and_then(|m| Path::new(m).parent())
                {
                    roots.push(root.to_path_buf());
                }
            }
        }
        roots.extend(sysroot);
        let roots = roots.iter().map(|r| resolved(r)).collect();
        Self {
            roots,
            base: base.to_path_buf(),
        }
    }

    /// Whether `file`, as the compiler named it, is one it may read: it
    /// is, once its links are followed, in one of the folders it may read
    /// (followed alike: a folder reached through a link, as macOS's
    /// `/var` is `/private/var`, or Windows' `\\?\` form of every path).
    fn allows(&self, file: &str) -> bool {
        // Names that are no file ("<anon>", "<macro expansion>"), and the
        // standard library as the compiler names it from its own build
        // ("/rustc/<hash>/library/…"), are nothing of the computer's —
        // with no `..` in them, which Windows takes out before it reads
        // ("/rustc/../Users/…" is `C:\Users\…` there).
        let parts = || file.split(['/', '\\']);
        if file.starts_with('<') && file.ends_with('>') && !file.contains(['/', '\\']) {
            return true;
        }
        if (file.starts_with("/rustc/") || file.starts_with("\\rustc\\"))
            && parts().all(|part| part != ".." && part != ".")
        {
            return true;
        }
        let path = Path::new(file);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base.join(path)
        };
        let real = resolved(&path);
        self.roots.iter().any(|root| real.starts_with(root))
    }
}

/// `path` with its links followed: as the system resolves it when it is
/// there; when it is not, its nearest folder that is, resolved, and the
/// rest as written, `..` taken out — so a file and the folders it may be in
/// are compared in the same form.
fn resolved(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let path = lexically(path);
    let mut rest = Vec::new();
    let mut at = path.as_path();
    loop {
        if let Ok(real) = at.canonicalize() {
            let mut out = real;
            out.extend(rest.iter().rev());
            return out;
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                at = parent;
            }
            _ => return path,
        }
    }
}

/// `path` with its `.` and `..` taken out, as written: for a file that is
/// not there to resolve.
fn lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The first file the build read that it may not: one a dep-info file of
/// this build names, or one a compiler message points into. What the reads
/// check (`reads`) can tell from the source is refused before; this is what
/// the compiler says it read, whatever put it together (a macro, a string a
/// derive reads as code), so nothing read outside comes back.
///
/// With it, the crate whose compiling read it, as the compiler names it.
fn outside_read(
    readable: &Readable,
    dep_infos: &[PathBuf],
    files: &[(String, String)],
) -> Option<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let named = dep_infos
        .iter()
        .filter(|d| seen.insert((*d).clone()))
        .filter_map(|d| Some((crate_of(d), std::fs::read_to_string(d).ok()?)))
        .flat_map(|(of, text)| {
            dep_info_files(&text)
                .into_iter()
                .map(move |file| (file, of.clone()))
        })
        .chain(files.iter().map(|(of, file)| (file.clone(), of.clone())));
    let mut checked = std::collections::HashSet::new();
    named
        .filter(|(file, _)| checked.insert(file.clone()))
        .find(|(file, _)| !readable.allows(file))
}

/// The crate a dep-info file is of: rustc names it `<crate>-<hash>.d`, a
/// build script's `build_script_build-<hash>.d` in its package's folder
/// (`<package>-<hash>`), and cargo's copy beside the plugin is unhashed.
fn crate_of(dep_info: &Path) -> String {
    fn unhashed(name: &str) -> &str {
        match name.rsplit_once('-') {
            Some((name, hash))
                if hash.len() >= 8 && hash.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                name
            }
            _ => name,
        }
    }
    let stem = dep_info
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = unhashed(&stem);
    if name.starts_with("build_script_") {
        let folder = dep_info
            .parent()
            .and_then(|p| p.file_name())
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        return unhashed(&folder).to_string();
    }
    if name == stem {
        // Unhashed: the plugin's own (a cdylib, which has no `lib` before
        // its name on wasm), rustc's or cargo's copy beside it.
        return name.to_string();
    }
    name.to_string()
}

/// The package `crate_name` is of, as plugin.toml and cargo name it, with
/// its version ("zerocopy 0.8.27"); `None` for the plugin itself.
fn package_of(metadata: &str, crate_name: &str, plugin: &str) -> Option<String> {
    let norm = |name: &str| name.replace('-', "_");
    let wanted = norm(crate_name);
    if wanted.is_empty() || wanted == norm(plugin) {
        return None;
    }
    let found = serde_json::from_str::<serde_json::Value>(metadata)
        .ok()
        .and_then(|metadata| {
            metadata["packages"]
                .as_array()?
                .iter()
                .find(|package| {
                    package["name"].as_str().is_some_and(|n| norm(n) == wanted)
                        || package["targets"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|t| t["name"].as_str().is_some_and(|n| norm(n) == wanted))
                })
                .map(|package| {
                    format!(
                        "{} {}",
                        package["name"].as_str().unwrap_or(crate_name),
                        package["version"].as_str().unwrap_or_default()
                    )
                    .trim_end()
                    .to_string()
                })
        });
    Some(found.unwrap_or_else(|| crate_name.to_string()))
}

/// The toolchain's own folder (`rustc --print sysroot`): its library's
/// sources, which messages may point into.
fn sysroot(toolchain: &Toolchain, cancel: &AtomicBool) -> Result<PathBuf, BuildError> {
    let mut cmd = clew_core::spawn::command(&toolchain.rustc);
    cmd.env_clear();
    for (name, value) in std::env::vars_os() {
        if name.to_str().is_some_and(|n| kept(n, false)) {
            cmd.env(name, value);
        }
    }
    cmd.args(["--print", "sysroot"]);
    let ran = run(cmd, Duration::from_secs(30), cancel)?;
    let path = ran.stdout.trim();
    if ran.success && !path.is_empty() {
        Ok(PathBuf::from(path))
    } else {
        Err(BuildError::Failed(format!(
            "rustc did not say where its library is:\n{}",
            last_lines(&ran.stderr, 5)
        )))
    }
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
    let built = build_with(source, work, toolchain, cancel);
    // The Rust it was built with removed meanwhile (Clew's own, from
    // Settings → Plugins or another window): said so, not as whatever
    // cargo said of a file it lost.
    match built {
        Err(BuildError::Source(_) | BuildError::Failed(_) | BuildError::Compile(_))
            if !toolchain.cargo.is_file() || !toolchain.rustc.is_file() =>
        {
            Err(rust_removed(toolchain))
        }
        built => built,
    }
}

/// Why a build failed when the Rust it used went away while it ran.
fn rust_removed(toolchain: &Toolchain) -> BuildError {
    match toolchain.cargo_home {
        Some(_) => BuildError::Toolchain {
            message: "Clew's own Rust was removed while the plugin was being built.".into(),
            fix: Some(
                "ask the user to open Clew's Settings → Plugins, where one button installs Rust into Clew's own folder again; then build again.".into(),
            ),
        },
        None => BuildError::Toolchain {
            message: format!(
                "The Rust the plugin was being built with ({}) was removed while it was built.",
                toolchain.cargo.display()
            ),
            fix: Some(
                "ask the user to install Rust again from https://rustup.rs, or to open Clew's Settings → Plugins, where one button installs Rust into Clew's own folder; then build again.".into(),
            ),
        },
    }
}

/// The plugin's build folder, one build of it at a time: another build of
/// the same plugin (another window's, another agent's) is waited for, for
/// as long as a build may take, and the wait ends when the build is
/// cancelled. Where locks cannot be had (a file system without them) it is
/// built without one, as Clew's other shared files are.
fn build_lock(
    crate_dir: &Path,
    id: &str,
    cancel: &AtomicBool,
) -> Result<std::fs::File, BuildError> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(crate_dir.join(".build.lock"))
        .map_err(|e| {
            BuildError::Failed(format!("the build folder of {id} could not be locked: {e}"))
        })?;
    let since = std::time::Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) | Err(std::fs::TryLockError::Error(_)) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {}
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(BuildError::Failed("cancelled".into()));
        }
        if since.elapsed() > 2 * METADATA_DEADLINE + BUILD_DEADLINE {
            return Err(BuildError::Failed(format!(
                "another build of {id} has been running for longer than a build may; build again when it has ended"
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn build_with(
    source: &Path,
    work: &Path,
    toolchain: &Toolchain,
    cancel: &AtomicBool,
) -> Result<Built, BuildError> {
    let id = read_manifest(source)?.plugin.id;
    let crate_dir = work.join(&id);
    std::fs::create_dir_all(&crate_dir).map_err(|e| BuildError::Failed(e.to_string()))?;
    let _exclusive = build_lock(&crate_dir, &id, cancel)?;

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

    // Each plugin's own: what a build compiled and read is checked as that
    // build's, and none of it is another plugin's to remove. A build
    // stopped before what it read was checked leaves nothing to build on.
    let target = work.join("targets").join(&id);
    if target.join(UNCHECKED).exists() {
        std::fs::remove_dir_all(&target).map_err(|e| BuildError::Failed(e.to_string()))?;
    }
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
    let metadata = ran.stdout.clone();
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
    // A cargo configuration's `[env]` reaches the build only through
    // variables it does not set already: each is set, empty.
    // Those the build is given already (PATH, HOME…) stay as given: a
    // configuration's `[env]` does not come before them, and emptied they
    // broke the build.
    // (Windows' names are one whatever their case; others' are not.)
    let named = |name: &str| match cfg!(windows) {
        true => name.to_ascii_uppercase(),
        false => name.to_string(),
    };
    let given: Vec<String> = build
        .get_envs()
        .map(|(name, _)| named(&name.to_string_lossy()))
        .collect();
    for name in configured_env(&crate_dir, toolchain.cargo_home.as_deref())? {
        if !given.contains(&named(&name)) {
            build.env(name, "");
        }
    }
    let sysroot = sysroot(toolchain, cancel)?;
    let before = dep_infos(&target);
    std::fs::create_dir_all(&target).map_err(|e| BuildError::Failed(e.to_string()))?;
    std::fs::write(target.join(UNCHECKED), "").map_err(|e| BuildError::Failed(e.to_string()))?;
    // With its folder there: what the build makes in it is compared with
    // the folder as resolved, not as written (a first build's folder was
    // not there to resolve, and every file it made was taken for outside).
    let readable = Readable::new(&target, &crate_dir, &metadata, Some(sysroot));
    let ran = run(build, BUILD_DEADLINE, cancel)?;
    let mut messages = read_messages(&ran.stdout, &id);
    // What the compiler read, before anything it said is used: a file of
    // the computer's, read by what the checks before could not see, would
    // come back in its messages or inside the plugin.
    messages
        .dep_infos
        .extend(written_dep_infos(&target, &before));
    if let Some((file, of)) = outside_read(&readable, &messages.dep_infos, &messages.files) {
        // Nothing of it is kept: what was compiled from it, cached for the
        // next build, goes with the rest of the build's folder. What could
        // not be removed (a file held open) is marked unchecked again, as
        // the removal may have taken the mark first: wiped by the next
        // build, not built on, its `.d` taken for one checked.
        if std::fs::remove_dir_all(&target).is_err() && target.exists() {
            let _ = std::fs::write(target.join(UNCHECKED), "");
        }
        let outside = format!(
            "{file}, which is outside the plugin and the packages it uses: a file of the computer's. Nothing the compiler said is passed on."
        );
        return Err(BuildError::Policy(match package_of(&metadata, &of, &id) {
            None => format!(
                "The plugin was not built: compiling it read {outside} Take out what reads it: a #[path], an include, or an attribute or a macro that puts one together."
            ),
            Some(package) => format!(
                "The plugin was not built: compiling {package}, a crate it uses, read {outside} Use another crate in place of it, or write what you need yourself."
            ),
        }));
    }
    let _ = std::fs::remove_file(target.join(UNCHECKED));
    if !ran.success {
        // A unit whose compiling stopped before it wrote its dep-info file
        // said nothing of what it read (a bracket left open, a macro's
        // recursion's end) — and a message of its from before the stop can
        // hold what it read (a `compile_error!` given an include). So none
        // of what the compiler said is passed on then.
        // Each failed unit is tied to its own dep-info file, written this
        // build (word for word as before or not): of its crate's name, and
        // compiled from the file it is (rustc names the crate's root first).
        // Not another crate's that read that file too (one handed another
        // package's lib.rs to include), nor a unit of its name cached and
        // not built now (another version of the crate).
        let own: Vec<(String, PathBuf)> = rewritten_dep_infos(&target, &before)
            .iter()
            .filter_map(|d| {
                let root = dep_info_root(&std::fs::read_to_string(d).ok()?)?;
                Some((
                    crate_of(d).replace('-', "_"),
                    resolved(&crate_dir.join(root)),
                ))
            })
            .collect();
        let unsaid = messages.failed.iter().find(|(unit, from)| {
            !from.as_ref().is_some_and(|from| {
                let from = resolved(&crate_dir.join(from));
                own.iter().any(|(of, root)| of == unit && *root == from)
            })
        });
        if let Some((unit, _)) = unsaid {
            return Err(unread_failure(
                &crate_dir,
                &id,
                unit,
                &metadata,
                &messages.stops,
            ));
        }
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
    record_built_from(&crate_dir, made_from, &bytes)
        .map_err(|e| BuildError::Failed(e.to_string()))?;
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
/// (`source_digest`) and the file it made (`built`, by its digest), so the
/// two are read as one; `None`, when the source could not be read, notes
/// none.
pub(crate) fn record_built_from(
    crate_dir: &Path,
    digest: Option<String>,
    built: &[u8],
) -> std::io::Result<()> {
    let stamp = crate_dir.join(SOURCE_STAMP);
    match digest {
        Some(digest) => std::fs::write(&stamp, format!("{digest} {}\n", artifact_digest(built))),
        None => std::fs::remove_file(&stamp).or(Ok(())),
    }
}

fn artifact_digest(built: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    crate::inspect::hex(&Sha256::digest(built))
}

/// Whether `built`, the file read of the build in `crate_dir`, is the build
/// of the plugin's source at `source` as it is now: what the build made
/// from it, and nothing written to it since. The file and its note are
/// written one after the other; read between, or while another build
/// writes them, one build's file never passes as another's.
pub fn built_from(crate_dir: &Path, source: &Path, built: &[u8]) -> bool {
    let Ok(stamp) = std::fs::read_to_string(crate_dir.join(SOURCE_STAMP)) else {
        return false;
    };
    let mut parts = stamp.split_whitespace();
    let (Some(from), Some(made), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    made == artifact_digest(built) && source_digest(source).is_some_and(|now| now == from)
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

    /// A dep-info file's paths, escaped spaces kept, `#` lines left out;
    /// and every file a compiler message's spans point into, its notes'
    /// and macro expansions' too.
    #[test]
    fn what_the_compiler_says_it_read_is_read_whole() {
        // A line begun with `#` that is none of rustc's is read for files:
        // a path with a line break in it is not half read. rustc's own are
        // the last lines, after an empty one; one so begun elsewhere (a
        // path's rest made to look like one) is read for files too.
        assert_eq!(
            dep_info_files("src/lib.rs: /a/x\n#y/../../etc/passwd\n\n# checksum:abc\n"),
            ["src/lib.rs", "/a/x", "#y/../../etc/passwd"]
        );
        assert_eq!(
            dep_info_files("src/lib.rs: /a/x\n# env-dep:/../../home/u/.netrc\n"),
            ["src/lib.rs", "/a/x", "#", "env-dep:/../../home/u/.netrc"]
        );
        assert_eq!(
            dep_info_files(
                "/t/deps/a-1.d: src/lib.rs /x\\ y/z.rs\n\nsrc/lib.rs:\n/x\\ y/z.rs:\n\n# env-dep:HOME=/root\n"
            ),
            [
                "/t/deps/a-1.d",
                "src/lib.rs",
                "/x y/z.rs",
                "src/lib.rs",
                "/x y/z.rs"
            ]
        );
        let message: serde_json::Value = serde_json::json!({
            "spans": [{"file_name": "src/lib.rs", "expansion": {"span": {"file_name": "/a/macro.rs"}}}],
            "children": [{"spans": [{"file_name": "/b/note.rs"}]}]
        });
        let mut files = Vec::new();
        span_files(&message, &mut files);
        files.sort();
        assert_eq!(files, ["/a/macro.rs", "/b/note.rs", "src/lib.rs"]);
    }

    /// Which crate read a file is said as the agent knows it: the plugin
    /// itself, or a package it uses with its version, whatever rustc named
    /// the dep-info file it is in (hashed, a build script's, cargo's copy).
    #[test]
    fn what_read_outside_is_named_by_its_package() {
        let h = "0123456789abcdef";
        for (dep_info, of) in [
            (
                format!("/t/wasm32-wasip2/release/deps/zerocopy-{h}.d"),
                "zerocopy",
            ),
            (format!("/t/wasm32-wasip2/release/deps/libc-{h}.d"), "libc"),
            (
                format!("/t/wasm32-wasip2/release/deps/wit_bindgen-{h}.d"),
                "wit_bindgen",
            ),
            (
                format!("/t/release/build/serde-json-{h}/build_script_build-{h}.d"),
                "serde-json",
            ),
            ("/t/wasm32-wasip2/release/sample.d".to_string(), "sample"),
            // A plugin's id may begin with "lib": a cdylib on wasm has no
            // `lib` before its name, so none is taken off.
            (
                "/t/wasm32-wasip2/release/deps/library_index.d".to_string(),
                "library_index",
            ),
        ] {
            assert_eq!(crate_of(Path::new(&dep_info)), of, "{dep_info}");
        }
        let metadata = serde_json::json!({"packages": [
            {"name": "wit-bindgen", "version": "0.57.1", "targets": [{"name": "wit_bindgen"}]},
            {"name": "serde-json", "version": "1.0.140", "targets": [{"name": "serde_json"}, {"name": "build-script-build"}]},
            {"name": "todo-authors", "version": "0.1.0", "targets": [{"name": "todo_authors"}]},
        ]})
        .to_string();
        assert_eq!(
            package_of(&metadata, "wit_bindgen", "todo-authors").as_deref(),
            Some("wit-bindgen 0.57.1")
        );
        assert_eq!(
            package_of(&metadata, "serde-json", "todo-authors").as_deref(),
            Some("serde-json 1.0.140")
        );
        assert_eq!(package_of(&metadata, "todo_authors", "todo-authors"), None);
        assert_eq!(package_of(&metadata, "", "todo-authors"), None);
        assert_eq!(
            package_of(&metadata, "unlisted", "todo-authors").as_deref(),
            Some("unlisted")
        );
    }

    /// The build may read its own folder, the packages it uses and the
    /// toolchain's library; a path that climbs out, a link out, or any
    /// other file of the computer's is not.
    #[test]
    fn what_a_build_may_read_is_its_own_its_packages_and_the_toolchains() {
        let root = clew_core::testutil::TempDir::new("plugin-readable");
        let (work, package, sysroot, outside) = (
            root.join("work"),
            root.join("registry/serde-1.0"),
            root.join("sysroot"),
            root.join("outside"),
        );
        for dir in [&work.join("sample/src"), &package, &sysroot, &outside] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(outside.join("secret"), "s").unwrap();
        let metadata = serde_json::json!({"packages": [
            {"manifest_path": package.join("Cargo.toml").to_string_lossy()}
        ]})
        .to_string();
        let readable = Readable::new(
            &work.join("target"),
            &work.join("sample"),
            &metadata,
            Some(sysroot.clone()),
        );
        for allowed in [
            "src/lib.rs".to_string(),
            work.join("target/out/gen.rs")
                .to_string_lossy()
                .into_owned(),
            package.join("src/lib.rs").to_string_lossy().into_owned(),
            sysroot
                .join("lib/rustlib/src/rust/library/core/src/lib.rs")
                .to_string_lossy()
                .into_owned(),
            "/rustc/0123abcd/library/core/src/macros/mod.rs".to_string(),
            "<anon>".to_string(),
        ] {
            assert!(readable.allows(&allowed), "{allowed}");
        }
        for refused in [
            outside.join("secret").to_string_lossy().into_owned(),
            "../../outside/secret".to_string(),
            work.join("other-plugin/src/lib.rs")
                .to_string_lossy()
                .into_owned(),
            "/etc/passwd".to_string(),
            // rustc's name for the standard library, climbing out of it: on
            // Windows `..` is taken out before the file is read.
            "/rustc/../etc/passwd".to_string(),
            "/rustc/0123abcd/../../home/u/.netrc".to_string(),
            // Windows reads `\rustc\..\Users\…` as `C:\Users\…` (on Unix it is
            // one name, of a file in the plugin's folder).
            #[cfg(windows)]
            "\\rustc\\..\\Users\\u\\secret.rs".to_string(),
            "<a/../../etc/passwd>".to_string(),
            // Of the toolchain's folder, its library only.
            sysroot.join("share/secret").to_string_lossy().into_owned(),
        ] {
            assert!(!readable.allows(&refused), "{refused}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("secret"), work.join("sample/src/link.rs"))
                .unwrap();
            assert!(!readable.allows("src/link.rs"), "a link out of the build");
        }
        // The folders reached through a link (macOS's `/var` is
        // `/private/var`), what the build makes there not made yet: in the
        // same form as the files, alike there or not.
        #[cfg(unix)]
        {
            let linked = root.join("linked");
            std::os::unix::fs::symlink(&work, &linked).unwrap();
            std::fs::create_dir_all(work.join("target")).unwrap();
            let readable = Readable::new(
                &linked.join("target"),
                &linked.join("sample"),
                &metadata,
                Some(sysroot.clone()),
            );
            for allowed in [
                linked.join("target/release/sample.wasm"),
                work.join("target/release/sample.wasm"),
                linked.join("sample/src/lib.rs"),
                work.join("sample/src/missing.rs"),
            ] {
                let allowed = allowed.to_string_lossy().into_owned();
                assert!(readable.allows(&allowed), "{allowed}");
            }
            let refused = linked.join("target/../../outside/secret");
            let refused = refused.to_string_lossy().into_owned();
            assert!(!readable.allows(&refused), "{refused}");
        }
    }

    /// Where the compiler stopped without saying what it read, Clew's own
    /// reading of the plugin's file says what does not parse there — in
    /// words of the file's own code — and nothing when it parses there, or
    /// the file is not the plugin's.
    #[test]
    fn where_the_compiler_stopped_clew_says_what_does_not_parse() {
        let dir = clew_core::testutil::TempDir::new("plugin-unparsed");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn f() {}\nlet x = 1;\n").unwrap();
        let said = unparsed_at(dir.path(), "src/lib.rs:2:1").expect("does not parse");
        assert!(said.starts_with("src/lib.rs:2:1: "), "{said}");
        assert!(said.contains("expected"), "{said}");
        assert_eq!(
            unparsed_at(dir.path(), "src/lib.rs:1:1"),
            None,
            "another line"
        );
        assert_eq!(unparsed_at(dir.path(), "../src/lib.rs:2:1"), None);
        assert_eq!(unparsed_at(dir.path(), "/etc/passwd:2:1"), None);
        assert_eq!(
            unparsed_at(dir.path(), "src\\lib.rs:2:1").as_deref(),
            Some(said.as_str()),
            "as rustc names it on Windows"
        );
        assert_eq!(unparsed_at(dir.path(), "src\\..\\src\\lib.rs:2:1"), None);
        std::fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        assert_eq!(unparsed_at(dir.path(), "src/lib.rs:1:1"), None, "it parses");
    }

    /// Code nested as deep as its tokens allow, every way Rust nests, is
    /// parsed without overflowing the stack (each would on a thread's
    /// usual one); a longer file is left to the compiler's own words.
    #[test]
    fn deep_code_is_parsed_without_overflowing_and_long_code_is_not_parsed() {
        let dir = clew_core::testutil::TempDir::new("plugin-unparsed-deep");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let fit = |head: &str, open: &str, middle: &str, close: &str, tail: &str| {
            let unparsed = "\nlet x = 1;\n";
            let fixed = crate::reads::tokens(&format!("{head}{middle}{tail}{unparsed}"));
            let per = crate::reads::tokens(&format!("{open}{close}"));
            let deep = (PARSED_TOKENS - fixed) / per;
            let text = format!(
                "{head}{}{middle}{}{tail}{unparsed}",
                open.repeat(deep),
                close.repeat(deep)
            );
            assert!(crate::reads::tokens(&text) > PARSED_TOKENS - per);
            assert!(crate::reads::tokens(&text) <= PARSED_TOKENS);
            text
        };
        for (what, text) in [
            ("unary", fit("fn f() { ", "-", "1", "", "; }")),
            ("brackets", fit("fn f() { ", "(", "1", ")", "; }")),
            ("arrays", fit("fn f() { ", "[", "1", "]", "; }")),
            ("blocks", fit("fn f() ", "{", "", "}", "")),
            ("closures", fit("fn f() { ", "|| ", "1", "", "; }")),
            ("references", fit("type T = ", "&", "u8", "", ";")),
            ("generics", fit("type T = ", "V<", "u8", ">", ";")),
            ("patterns", fit("fn f() { let ", "(", "x", ",)", " = 1; }")),
            ("modules", fit("", "mod m {", "", "}", "")),
            ("bounds", fit("fn f() -> ", "impl T<", "u8", ">", " {}")),
            ("qualified", fit("type T = ", "<", "u8", " as T>::A", ";")),
        ] {
            std::fs::write(dir.join("src/lib.rs"), &text).unwrap();
            let stop = format!("src/lib.rs:{}:1", text.lines().count());
            let said = unparsed_at(dir.path(), &stop);
            assert!(
                said.as_ref().is_some_and(|said| said.contains("expected")),
                "{what}: {said:?}"
            );
        }
        let long = "fn f() {}\n".repeat(PARSED_TOKENS / 6 + 1) + "let x = 1;\n";
        std::fs::write(dir.join("src/lib.rs"), &long).unwrap();
        let stop = format!("src/lib.rs:{}:1", long.lines().count());
        assert_eq!(unparsed_at(dir.path(), &stop), None, "too long to parse");
    }

    /// A unit of the build is named alike in a compiler message and by its
    /// dep-info file: a crate by its name, a build script by its package.
    #[test]
    fn a_unit_is_named_as_its_dep_info_file_names_it() {
        let lib = serde_json::json!({"target": {"name": "word-count", "kind": ["cdylib"]}});
        assert_eq!(unit_of(&lib), "word_count");
        assert_eq!(
            crate_of(Path::new("/t/deps/word_count-0123abcd0123abcd.d")),
            "word_count"
        );
        for id in [
            "registry+https://github.com/rust-lang/crates.io-index#serde-json@1.0.0",
            "serde-json 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)",
        ] {
            let script = serde_json::json!({
                "package_id": id,
                "target": {"name": "build-script-build", "kind": ["custom-build"]}
            });
            assert_eq!(unit_of(&script), "serde_json", "{id}");
        }
        assert_eq!(
            crate_of(Path::new(
                "/t/build/serde-json-0123abcd0123abcd/build_script_build-0123abcd0123abcd.d"
            ))
            .replace('-', "_"),
            "serde_json"
        );
    }

    /// The whole build, with a cargo that reports what a real one would:
    /// a dep-info naming a file outside, or a message pointing into one, is
    /// refused with nothing the compiler said passed on, and the plugin's
    /// build folder (what was compiled from it) goes. One left by a build
    /// stopped before its check is not built on, even written again word
    /// for word; another plugin's, just written, is not this one's. cargo
    /// is given none of the user's variables, no configured flags or
    /// wrapper, and a configured `[env]` emptied; one forced is refused.
    #[cfg(unix)]
    #[test]
    fn what_the_compiler_read_outside_the_build_is_refused_and_nothing_of_it_kept() {
        use std::os::unix::fs::PermissionsExt;
        let root = clew_core::testutil::TempDir::new("plugin-build-reads-checked");
        let (fake, outside, source, work, home, sysroot) = (
            root.join("fake"),
            root.join("outside"),
            root.join("source"),
            root.join("work"),
            root.join("cargo-home"),
            root.join("sysroot"),
        );
        for dir in [&fake, &outside, &source.join("src"), &home, &sysroot] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(outside.join("secret.txt"), "machine x password y").unwrap();
        std::fs::write(
            source.join("plugin.toml"),
            tests_support::manifest().to_toml(),
        )
        .unwrap();
        std::fs::write(source.join("src/lib.rs"), "// a plugin").unwrap();
        let script = |name: &str, text: String| {
            let path = fake.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let f = fake.display();
        let cargo = script(
            "cargo",
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  fetch) exit 0 ;;\n  metadata) echo '{{\"packages\":[]}}'; exit 0 ;;\n  build)\n    env > {f}/env\n    mkdir -p \"$CARGO_TARGET_DIR/wasm32-wasip2/release/deps\"\n    cp {f}/sample.d \"$CARGO_TARGET_DIR/wasm32-wasip2/release/deps/$(cat {f}/unit).d\"\n    cat {f}/messages\n    exit $(cat {f}/status) ;;\nesac\n"
            ),
        );
        let rustc = script("rustc", format!("#!/bin/sh\necho {}\n", sysroot.display()));
        let toolchain = Toolchain {
            cargo,
            rustc,
            version: "1.95.0".into(),
            cargo_home: Some(home.clone()),
        };
        let _env = clew_core::testutil::EnvVars::new()
            .set("NIX_CONFIG", "access-tokens = github.com=ghp_secret")
            .set("NIX_CFLAGS_COMPILE", "-O1")
            .set("CLEW_TEST_SECRET", "s3cret");
        let target = work.join("targets/sample");
        // As cargo names it: the file the plugin's crate is compiled from.
        let src_path = work.join("sample/src/lib.rs").display().to_string();
        let message = |file: &str, text: &str| {
            serde_json::json!({
                "reason": "compiler-message",
                "target": {"name": "sample", "src_path": src_path},
                "message": {"level": "error", "rendered": text, "spans": [{"file_name": file}]}
            })
            .to_string()
        };
        // As rustc writes it: the file the crate is compiled from first.
        let own = format!("{}/sample.d: src/lib.rs\n\nsrc/lib.rs:\n", target.display());
        std::fs::write(fake.join("unit"), "sample").unwrap();
        let attempt = |dep_info: String, messages: String| {
            std::fs::write(fake.join("sample.d"), dep_info).unwrap();
            std::fs::write(fake.join("messages"), messages + "\n").unwrap();
            std::fs::write(fake.join("status"), "101").unwrap();
            build(&source, &work, &toolchain, &AtomicBool::new(false))
        };
        let secret = outside.join("secret.txt").display().to_string();

        // A dep-info naming a file outside.
        let refused = attempt(
            format!("{}/sample.d: src/lib.rs {secret}\n", target.display()),
            message(
                "src/lib.rs",
                "error: expected item, found `machine x password y`",
            ),
        );
        assert!(
            matches!(&refused, Err(BuildError::Policy(why)) if why.contains(&secret) && !why.contains("password")),
            "{refused:?}"
        );
        assert!(!target.exists(), "what was compiled from it is kept");
        let env = std::fs::read_to_string(fake.join("env")).unwrap();
        for given in [
            "CARGO_ENCODED_RUSTFLAGS=\n".to_string(),
            "RUSTC_WRAPPER=\n".to_string(),
            "NIX_CFLAGS_COMPILE=-O1\n".to_string(),
            // What rustc writes stays in the plugin's folder, checked.
            format!("CARGO_BUILD_BUILD_DIR={}\n", target.display()),
            format!(
                "CARGO_BUILD_DEP_INFO_BASEDIR={}\n",
                work.join("sample").display()
            ),
        ] {
            assert!(env.contains(&given), "{given}: {env}");
        }
        assert!(
            !env.contains("NIX_CONFIG") && !env.contains("s3cret"),
            "{env}"
        );

        // A message pointing into a file outside.
        let refused = attempt(
            own.clone(),
            message(&secret, "error: expected item, found `machine`"),
        );
        assert!(
            matches!(&refused, Err(BuildError::Policy(why)) if why.contains(&secret)),
            "{refused:?}"
        );

        // All inside: the compiler's errors, as it said them.
        let compiled = attempt(
            own.clone(),
            message("src/lib.rs", "error[E0425]: cannot find value `x`"),
        );
        assert!(
            matches!(&compiled, Err(BuildError::Compile(errors)) if errors[0].contains("E0425")),
            "{compiled:?}"
        );

        // A build stopped before its check (its marker left): what it
        // wrote is not built on, though the next build writes it again
        // word for word.
        let stale = format!("{}/sample.d: src/lib.rs {secret}\n", target.display());
        std::fs::create_dir_all(target.join("wasm32-wasip2/release/deps")).unwrap();
        std::fs::write(target.join("wasm32-wasip2/release/deps/sample.d"), &stale).unwrap();
        std::fs::write(target.join(UNCHECKED), "").unwrap();
        let refused = attempt(stale, message("src/lib.rs", "error: expected item"));
        assert!(
            matches!(&refused, Err(BuildError::Policy(why)) if why.contains(&secret)),
            "{refused:?}"
        );

        // Checked, its marker goes; built again unchanged, it is not read
        // again as this build's, and what the compiler said is said again.
        for _ in 0..2 {
            let checked = attempt(own.clone(), message("src/lib.rs", "error[E0425]: x"));
            assert!(
                matches!(&checked, Err(BuildError::Compile(errors)) if errors[0].contains("E0425")),
                "{checked:?}"
            );
            assert!(!target.join(UNCHECKED).exists());
        }

        // The compiler stopped before it wrote what it read (a recursion's
        // end after a `compile_error!` handed an include): none of its
        // words are passed on — where it stopped, and an unclosed bracket
        // as Clew reads the plugin's files, are.
        let stopped = |file: &str| {
            serde_json::json!({
                "reason": "compiler-message",
                "target": {"name": "sample", "src_path": src_path},
                "message": {
                    "level": "error",
                    "rendered": "error: machine x password y",
                    "spans": [{"file_name": file, "is_primary": true, "line_start": 3, "column_start": 5}]
                }
            })
            .to_string()
        };
        let unwritten = |messages: String| {
            let _ = std::fs::remove_file(fake.join("sample.d"));
            std::fs::write(fake.join("messages"), messages + "\n").unwrap();
            std::fs::write(fake.join("status"), "101").unwrap();
            build(&source, &work, &toolchain, &AtomicBool::new(false))
        };
        // A unit of the same name cached and not built now (another version
        // of the crate, its dep-info file from an earlier build) does not
        // say what this one read.
        let cached = serde_json::json!({
            "reason": "compiler-artifact",
            "target": {"name": "sample", "src_path": "/elsewhere/sample-1.0/src/lib.rs"},
            "filenames": [target.join("wasm32-wasip2/release/deps/sample.wasm")]
        })
        .to_string();
        assert!(target.join("wasm32-wasip2/release/deps/sample.d").is_file());
        let withheld = unwritten(format!("{cached}\n{}", stopped("src/lib.rs")));
        assert!(
            matches!(&withheld, Err(BuildError::Compile(errors))
                if errors.len() == 1
                    && errors[0].contains("src/lib.rs:3:5: the compiler stopped here")
                    && !errors[0].contains("password")),
            "{withheld:?}"
        );
        std::fs::write(source.join("src/lib.rs"), "fn f() {\n    let x = (1;\n}\n").unwrap();
        let unpaired = unwritten(stopped("src/lib.rs"));
        assert!(
            matches!(&unpaired, Err(BuildError::Compile(errors))
                if errors[0].starts_with("error: src/lib.rs:3: this `}` closes the `(` of line 2, which `)` closes")
                    && !errors[0].contains("password")),
            "{unpaired:?}"
        );
        std::fs::write(source.join("src/lib.rs"), "// a plugin").unwrap();

        // What another crate read, though it names the plugin's file first,
        // does not say what the plugin's crate read; nor does a dep-info of
        // the plugin's name compiled from another file.
        let elsewhere = |unit: &str, dep_info: String| {
            std::fs::write(fake.join("unit"), unit).unwrap();
            let built = attempt(dep_info, stopped("src/lib.rs"));
            std::fs::write(fake.join("unit"), "sample").unwrap();
            built
        };
        for (unit, root) in [("helper", "src/lib.rs"), ("sample", "src/other.rs")] {
            let dep_info = format!("{}/{unit}.d: {root}\n\n{root}:\n", target.display());
            let withheld = elsewhere(unit, dep_info);
            assert!(
                matches!(&withheld, Err(BuildError::Compile(errors))
                    if errors[0].contains("src/lib.rs:3:5: the compiler stopped here")
                        && !errors[0].contains("password")),
                "{unit}: {withheld:?}"
            );
        }
        let said = elsewhere("sample", own.clone());
        assert!(
            matches!(&said, Err(BuildError::Compile(errors)) if errors[0].contains("password")),
            "its own: {said:?}"
        );

        // Another plugin built just after, in the same folder: its own
        // build folder, and the first plugin's files are none of its.
        let other = root.join("other");
        std::fs::create_dir_all(other.join("src")).unwrap();
        std::fs::write(
            other.join("plugin.toml"),
            tests_support::manifest()
                .to_toml()
                .replace("id = \"sample\"", "id = \"other\""),
        )
        .unwrap();
        std::fs::write(other.join("src/lib.rs"), "// another").unwrap();
        std::fs::write(
            fake.join("sample.d"),
            format!(
                "{}/other.d: src/lib.rs\n\nsrc/lib.rs:\n",
                work.join("targets/other").display()
            ),
        )
        .unwrap();
        std::fs::write(fake.join("unit"), "other").unwrap();
        std::fs::write(
            fake.join("messages"),
            serde_json::json!({
                "reason": "compiler-message",
                "target": {"name": "other", "src_path": work.join("other/src/lib.rs")},
                "message": {"level": "error", "rendered": "error[E0425]: y", "spans": [{"file_name": "src/lib.rs"}]}
            })
            .to_string()
                + "\n",
        )
        .unwrap();
        let built = build(&other, &work, &toolchain, &AtomicBool::new(false));
        std::fs::write(fake.join("unit"), "sample").unwrap();
        assert!(
            matches!(&built, Err(BuildError::Compile(errors)) if errors[0].contains("E0425")),
            "{built:?}"
        );
        assert!(work.join("targets/other").is_dir() && target.is_dir());

        // The Rust it builds with removed while it builds (Clew's own,
        // from another window): said so, not cargo's words for a file gone.
        let removing = script(
            "cargo-removing",
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  fetch) exit 0 ;;\n  metadata) echo '{{\"packages\":[]}}'; exit 0 ;;\n  build) rm {f}/rustc; echo 'error: could not exec rustc' >&2; exit 101 ;;\nesac\n"
            ),
        );
        let gone = build(
            &source,
            &work,
            &Toolchain {
                cargo: removing,
                ..toolchain.clone()
            },
            &AtomicBool::new(false),
        );
        assert!(
            matches!(&gone, Err(BuildError::Toolchain { message, .. }) if message.contains("removed while the plugin was being built")),
            "{gone:?}"
        );
        script("rustc", format!("#!/bin/sh\necho {}\n", sysroot.display()));

        // Another build of the plugin holding its folder: waited for, and
        // the wait ends when this one is cancelled.
        let holder = std::fs::OpenOptions::new()
            .write(true)
            .open(work.join("sample/.build.lock"))
            .unwrap();
        holder.lock().unwrap();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let stop = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                cancel.store(true, Ordering::Relaxed);
            })
        };
        let started = std::time::Instant::now();
        let waited = build(&source, &work, &toolchain, &cancel);
        stop.join().unwrap();
        assert!(
            matches!(&waited, Err(BuildError::Failed(why)) if why == "cancelled"),
            "{waited:?}"
        );
        assert!(started.elapsed() >= Duration::from_millis(250), "it waited");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "and stopped when cancelled"
        );
        drop(holder);

        // A configured [env]: emptied; one forced: refused.
        std::fs::write(
            home.join("config.toml"),
            "[env]\nCLEW_TEST_SECRET = \"from-config\"\nPATH = \"/nowhere\"\n",
        )
        .unwrap();
        let _ = attempt(own.clone(), String::new());
        let env = std::fs::read_to_string(fake.join("env")).unwrap();
        assert!(env.contains("CLEW_TEST_SECRET=\n"), "{env}");
        // One the build is given already stays as given.
        assert!(
            env.lines()
                .any(|l| l.starts_with("PATH=") && l.len() > "PATH=".len()),
            "{env}"
        );
        std::fs::write(
            home.join("config.toml"),
            "[env]\nCLEW_TEST_SECRET = { value = \"x\", force = true }\n",
        )
        .unwrap();
        let refused = attempt(own.clone(), String::new());
        assert!(
            matches!(&refused, Err(BuildError::Policy(why)) if why.contains("force = true")),
            "{refused:?}"
        );
    }

    #[test]
    fn a_build_is_of_the_source_as_it_was_built() {
        let root = clew_core::testutil::TempDir::new("plugin-build-from");
        let (source, built) = (root.join("source"), root.join("built"));
        std::fs::create_dir_all(source.join("src/parts")).unwrap();
        std::fs::create_dir_all(&built).unwrap();
        std::fs::write(source.join("plugin.toml"), "[plugin]\n").unwrap();
        std::fs::write(source.join("src/lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(source.join("src/parts/b.rs"), "fn b() {}\n").unwrap();
        let wasm = b"the plugin's file";
        // Never built.
        assert!(!built_from(&built, &source, wasm));
        record_built_from(&built, source_digest(&source), wasm).unwrap();
        assert!(built_from(&built, &source, wasm));
        // Another build's file, read as this one's note was written (or
        // this one's, read before another build's note): not this build.
        assert!(!built_from(&built, &source, b"another build's file"));
        // What a build does not stage does not change it.
        std::fs::write(source.join("src/notes.md"), "notes").unwrap();
        std::fs::write(source.join("src/.hidden.rs"), "fn h() {}").unwrap();
        std::fs::write(source.join("README.md"), "readme").unwrap();
        assert!(built_from(&built, &source, wasm));
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
            record_built_from(&built, before.clone(), wasm).unwrap();
            change();
            assert!(!built_from(&built, &source, wasm), "change {n}");
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
        assert!(!built_from(&built, &source, wasm));
        record_built_from(&built, None, wasm).unwrap();
        assert!(!built.join(SOURCE_STAMP).exists());
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
        let root = clew_core::testutil::TempDir::new("plugin-stage");
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
        let root = clew_core::testutil::TempDir::new("plugin-build-reads");
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
        let root = clew_core::testutil::TempDir::new("plugin-build-own");
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
